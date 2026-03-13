use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;

use super::BroadcastSink;

/// Run the UDS listener in a loop, accepting new trace clients.
/// Blocks until `stop_rx` fires or the listener is dropped.
pub fn run_listener(
    sock_path: &Path,
    broadcast: Arc<BroadcastSink>,
    stop_rx: crossbeam_channel::Receiver<()>,
) {
    // Clean up stale socket
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }

    let listener = match UnixListener::bind(sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[clitrace:daemon] Failed to bind {}: {}", sock_path.display(), e);
            return;
        }
    };

    // Set non-blocking so we can check stop_rx periodically
    listener.set_nonblocking(true).ok();

    eprintln!("[clitrace:daemon] Listening on {}", sock_path.display());

    loop {
        // Check for stop signal
        if stop_rx.try_recv().is_ok() {
            break;
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                // Set back to blocking for the client stream
                stream.set_nonblocking(false).ok();
                let count = broadcast.client_count() + 1;
                eprintln!("[clitrace:daemon] Client connected ({} total)", count);
                broadcast.add_client(stream);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No pending connection — sleep briefly and retry
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[clitrace:daemon] Accept error: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    // Cleanup socket file on exit
    let _ = std::fs::remove_file(sock_path);
    eprintln!("[clitrace:daemon] Listener stopped");
}
