use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::common::schema::ProviderSchema;
use crate::common::types::{ModelUsageSummary, UsageEventWithTs};
use crate::pricing::PricingTable;
use crate::sink::Sink;
use crate::sink::json::{event_to_json, events_batch_to_json, grouped_to_json, summaries_to_json};

/// Maximum local queue size per client. If exceeded, client is disconnected.
const MAX_QUEUE_SIZE: usize = 1024;

/// Shared state: single message slot + seq counter.
/// Engine writes here, notify_all wakes all receiver threads.
struct SharedState {
    seq: u64,
    message: String,
    closed: bool,
}

/// Broadcast sink using Condvar::notify_all with per-worker local queues.
///
/// Architecture (per client, 2 threads):
///   Thread A (receiver): condvar.wait_timeout → clone msg → local queue push → wait
///   Thread B (writer):   queue pop → batch write_all
///
/// Engine is O(1): one write + notify_all regardless of client count.
/// Clients are fully independent. No data loss under normal operation.
pub struct BroadcastSink {
    state: Arc<Mutex<SharedState>>,
    condvar: Arc<Condvar>,
    client_count: Arc<AtomicUsize>,
}

impl Default for BroadcastSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Peek for EOF without consuming data: a disconnected peer returns 0.
/// Used to reap trace clients on an idle daemon (no writes = no write errors).
fn peer_disconnected(stream: &UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut byte = 0u8;
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            &mut byte as *mut u8 as *mut libc::c_void,
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n == 0
}

impl BroadcastSink {
    pub fn new() -> Self {
        BroadcastSink {
            state: Arc::new(Mutex::new(SharedState {
                seq: 0,
                message: String::new(),
                closed: false,
            })),
            condvar: Arc::new(Condvar::new()),
            client_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Maximum concurrent trace subscribers. TRACE releases its listener
    /// worker permit immediately (the stream lives on in these two threads),
    /// so this is the only bound on trace resource use.
    pub const MAX_CLIENTS: usize = 16;

    /// Subscribe a new trace client. Spawns two threads:
    ///   - Receiver: waits on condvar, pushes to local queue
    ///   - Writer: pops from local queue, writes to UDS
    ///
    /// Returns false when the subscriber cap is already reached (the caller
    /// should reject the connection). The check is a CAS, not check-then-act:
    /// several listener workers can race here.
    pub fn add_client(&self, stream: UnixStream) -> bool {
        let state = Arc::clone(&self.state);
        let condvar = Arc::clone(&self.condvar);
        let count = Arc::clone(&self.client_count);

        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));

        // Atomic admit: fetch_add + check would transiently exceed the cap.
        if count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| {
                if c >= Self::MAX_CLIENTS { None } else { Some(c + 1) }
            })
            .is_err()
        {
            return false;
        }

        // Local queue: receiver pushes, writer pops
        let queue = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let queue_condvar = Arc::new(Condvar::new());
        // Shared flag: writer sets to false on write failure, receiver checks
        let alive = Arc::new(AtomicBool::new(true));

        // Snapshot current seq
        let last_seq = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.seq
        };

        // Thread A: receiver — condvar.wait_timeout → clone → queue push → wait
        let recv_result = {
            let queue = Arc::clone(&queue);
            let queue_condvar = Arc::clone(&queue_condvar);
            let alive = Arc::clone(&alive);
            let count = Arc::clone(&count);
            let mut last_seq = last_seq;

            std::thread::Builder::new()
                .name("toki-trace-recv".to_string())
                .spawn(move || {
                    let timeout = std::time::Duration::from_secs(5);
                    loop {
                        let message = {
                            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                            while s.seq == last_seq && !s.closed && alive.load(Ordering::Relaxed) {
                                let (guard, _) = condvar.wait_timeout(s, timeout)
                                    .unwrap_or_else(|e| e.into_inner());
                                s = guard;
                            }
                            if s.closed || !alive.load(Ordering::Relaxed) {
                                break;
                            }
                            last_seq = s.seq;
                            s.message.clone()
                        };

                        if !alive.load(Ordering::Relaxed) {
                            break;
                        }

                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        if q.len() >= MAX_QUEUE_SIZE {
                            alive.store(false, Ordering::Relaxed);
                            queue_condvar.notify_one();
                            break;
                        }
                        q.push_back(message);
                        queue_condvar.notify_one();
                    }
                    count.fetch_sub(1, Ordering::Relaxed);
                })
        };

        if let Err(e) = recv_result {
            eprintln!("[toki:daemon] Failed to spawn receiver thread: {}", e);
            count.fetch_sub(1, Ordering::Relaxed);
            let _ = writeln!(&stream, "{{\"error\":\"server thread spawn failed\"}}");
            return true;
        }

        // Thread B: writer — queue pop → batch write_all
        let write_result = {
            let queue = Arc::clone(&queue);
            let queue_condvar = Arc::clone(&queue_condvar);
            let alive = Arc::clone(&alive);
            // The receiver parks on the SHARED broadcast condvar; wake it whenever
            // this writer flips `alive` false (e.g. the client disconnects and
            // write_all fails) so client_count drops promptly instead of lingering
            // until the receiver's 5s wait_timeout expires.
            let broadcast_condvar = Arc::clone(&self.condvar);
            let mut stream = stream;

            std::thread::Builder::new()
                .name("toki-trace-write".to_string())
                .spawn(move || {
                    let mut batch = Vec::new();
                    loop {
                        {
                            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                            // Bounded wait: a disconnect is only noticed when a
                            // write fails, and on an idle daemon no write ever
                            // happens — dead clients would hold their slot (and
                            // two threads) until traffic resumed, eventually
                            // denying the subscriber cap to live clients.
                            while q.is_empty() && alive.load(Ordering::Relaxed) {
                                let (guard, timeout) = queue_condvar
                                    .wait_timeout(q, std::time::Duration::from_secs(5))
                                    .unwrap_or_else(|e| e.into_inner());
                                q = guard;
                                if timeout.timed_out() && q.is_empty() && peer_disconnected(&stream) {
                                    alive.store(false, Ordering::Relaxed);
                                    broadcast_condvar.notify_all();
                                    break;
                                }
                            }
                            if !alive.load(Ordering::Relaxed) && q.is_empty() {
                                break;
                            }
                            batch.extend(q.drain(..));
                        }

                        let mut buf = String::new();
                        for msg in batch.drain(..) {
                            buf.push_str(&msg);
                            buf.push('\n');
                        }
                        if stream.write_all(buf.as_bytes()).is_err() {
                            alive.store(false, Ordering::Relaxed);
                            broadcast_condvar.notify_all();
                            break;
                        }
                    }
                })
        };

        if let Err(e) = write_result {
            eprintln!("[toki:daemon] Failed to spawn writer thread: {}", e);
            alive.store(false, Ordering::Relaxed);
            let _ = &e;
            // Wake the receiver immediately so it observes alive=false and exits,
            // instead of lingering until the 5s condvar timeout. The receiver
            // parks on the shared broadcast condvar, so notify that.
            self.condvar.notify_all();
        }
        true
    }

    pub fn client_count(&self) -> usize {
        self.client_count.load(Ordering::Relaxed)
    }

    /// Publish a JSON message. O(1) — writes to shared state + notify_all.
    fn broadcast(&self, json: &serde_json::Value) {
        if self.client_count() == 0 {
            return;
        }
        let line = serde_json::to_string(json).unwrap_or_default();
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.seq += 1;
            s.message = line;
        }
        self.condvar.notify_all();
    }
}

impl Drop for BroadcastSink {
    fn drop(&mut self) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.closed = true;
        }
        self.condvar.notify_all();
    }
}

impl Sink for BroadcastSink {
    fn emit_event(&self, event: &UsageEventWithTs, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        self.broadcast(&event_to_json(event, pricing, schema));
    }

    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        self.broadcast(&summaries_to_json(summaries, pricing, schema));
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        self.broadcast(&grouped_to_json(grouped, type_name, pricing, schema));
    }

    fn emit_list(&self, items: &[String], type_name: &str) {
        self.broadcast(&serde_json::json!({ "type": type_name, "items": items }));
    }

    fn emit_events_batch(&self, events: &[crate::common::types::RawEvent], pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        self.broadcast(&events_batch_to_json(events, pricing, schema));
    }
}

impl Sink for Arc<BroadcastSink> {
    fn emit_event(&self, event: &UsageEventWithTs, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        (**self).emit_event(event, pricing, schema);
    }

    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        (**self).emit_summary(summaries, pricing, schema);
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        (**self).emit_grouped(grouped, type_name, pricing, schema);
    }

    fn emit_list(&self, items: &[String], type_name: &str) {
        (**self).emit_list(items, type_name);
    }

    fn emit_events_batch(&self, events: &[crate::common::types::RawEvent], pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        (**self).emit_events_batch(events, pricing, schema);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    #[test]
    fn writer_death_wakes_receiver_promptly() {
        // A trace client disconnects; the writer's next write_all fails and flips
        // `alive` false. The receiver parks on the SHARED broadcast condvar, so it
        // must be woken by the writer's notify — not linger until the 5s
        // wait_timeout — and client_count must fall to 0 well inside that window.
        let sink = BroadcastSink::new();
        let (a, b) = UnixStream::pair().unwrap();
        sink.add_client(a);
        assert_eq!(sink.client_count(), 1);

        // Client goes away.
        drop(b);

        // Publish until the writer observes the broken pipe. Each broadcast wakes
        // the receiver → writer, so the failed write is reached quickly. The whole
        // teardown must complete far under the 5s receiver timeout.
        let start = Instant::now();
        loop {
            sink.broadcast(&serde_json::json!({ "x": 1 }));
            if sink.client_count() == 0 {
                break;
            }
            if start.elapsed() > Duration::from_secs(3) {
                panic!("client_count did not drop after writer death (receiver stuck on condvar)");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(start.elapsed() < Duration::from_secs(4), "teardown must beat the 5s timeout");
    }
}
