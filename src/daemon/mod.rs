mod broadcast;
mod listener;
mod pidfile;

pub use broadcast::BroadcastSink;
pub use listener::run_listener;
pub use pidfile::{write_pidfile, read_pidfile, remove_pidfile, default_pidfile_path};

use std::path::PathBuf;

/// Default daemon socket path.
pub fn default_sock_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("toki").join("daemon.sock")
}

/// Send SIGTERM to the daemon process via PID file.
/// Returns Ok(true) if signal sent, Ok(false) if not running.
pub fn stop_daemon(pidfile: &std::path::Path, sock: &std::path::Path) -> Result<bool, String> {
    match read_pidfile(pidfile) {
        Some(pid) => {
            // Check if process is alive
            let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
            if !alive {
                // Stale PID file — clean up
                remove_pidfile(pidfile);
                let _ = std::fs::remove_file(sock);
                return Ok(false);
            }
            // Send SIGTERM for graceful shutdown
            unsafe { libc::kill(pid as i32, libc::SIGTERM); }

            // Wait for process to actually exit (up to 10s graceful, then SIGKILL)
            let mut exited = false;
            for i in 0..100 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if unsafe { libc::kill(pid as i32, 0) != 0 } {
                    exited = true;
                    break;
                }
                // After 10s, escalate to SIGKILL
                if i == 99 {
                    eprintln!("[toki] Graceful shutdown timed out, forcing...");
                    unsafe { libc::kill(pid as i32, libc::SIGKILL); }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    exited = unsafe { libc::kill(pid as i32, 0) != 0 };
                }
            }

            if !exited {
                eprintln!("[toki] Warning: process {} may still be alive", pid);
            }

            remove_pidfile(pidfile);
            let _ = std::fs::remove_file(sock);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Check if daemon is running. Returns Some(pid) if alive.
pub fn daemon_status(pidfile: &std::path::Path) -> Option<u32> {
    let pid = read_pidfile(pidfile)?;
    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
    if alive { Some(pid) } else { None }
}
