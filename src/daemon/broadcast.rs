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
use crate::sink::json::{event_to_json, grouped_to_json, summaries_to_json};

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

    /// Subscribe a new trace client. Spawns two threads:
    ///   - Receiver: waits on condvar, pushes to local queue
    ///   - Writer: pops from local queue, writes to UDS
    pub fn add_client(&self, stream: UnixStream) {
        let state = Arc::clone(&self.state);
        let condvar = Arc::clone(&self.condvar);
        let count = Arc::clone(&self.client_count);

        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));

        count.fetch_add(1, Ordering::Relaxed);

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
            return;
        }

        // Thread B: writer — queue pop → batch write_all
        let write_result = {
            let queue = Arc::clone(&queue);
            let queue_condvar = Arc::clone(&queue_condvar);
            let alive = Arc::clone(&alive);
            let mut stream = stream;

            std::thread::Builder::new()
                .name("toki-trace-write".to_string())
                .spawn(move || {
                    let mut batch = Vec::new();
                    loop {
                        {
                            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                            while q.is_empty() && alive.load(Ordering::Relaxed) {
                                q = queue_condvar.wait(q).unwrap_or_else(|e| e.into_inner());
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
                            break;
                        }
                    }
                })
        };

        if let Err(e) = write_result {
            eprintln!("[toki:daemon] Failed to spawn writer thread: {}", e);
            alive.store(false, Ordering::Relaxed);
            // receiver thread will exit on next wake via alive check
        }
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
}
