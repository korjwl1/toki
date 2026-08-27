mod broadcast;
mod listener;
mod pidfile;

pub use broadcast::BroadcastSink;
pub use listener::run_listener;
pub use pidfile::{write_pidfile, read_pidfile, remove_pidfile, default_pidfile_path};

use std::path::PathBuf;

/// Default daemon socket path.
pub fn default_sock_path() -> PathBuf {
    let home = crate::config::home_dir();
    home.join(".config").join("toki").join("daemon.sock")
}

/// Best-effort guard against signaling an unrelated process that reused a stale
/// PID. Returns `false` only when we can positively confirm the process is NOT
/// toki; if `ps` is unavailable or its output is empty/ambiguous it returns
/// `true`, preserving prior behavior (never makes us miss a real toki process).
fn pid_looks_like_toki(pid: u32) -> bool {
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
    {
        Ok(out) if out.status.success() => {
            let comm = String::from_utf8_lossy(&out.stdout);
            let comm = comm.trim();
            if comm.is_empty() {
                return true; // can't determine
            }
            // `comm` may be a full path on macOS; match on the basename.
            let base = comm.rsplit('/').next().unwrap_or(comm);
            base.contains("toki")
        }
        _ => true, // ps failed → don't change behavior
    }
}

/// Send SIGTERM to the daemon process via PID file.
/// Returns Ok(true) if signal sent, Ok(false) if not running.
pub fn stop_daemon(pidfile: &std::path::Path, sock: &std::path::Path) -> Result<bool, String> {
    match read_pidfile(pidfile) {
        Some(pid) => {
            // Check if process is alive AND is actually toki (guards against a
            // reused PID after a crash + stale pidfile).
            let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
            if !alive || !pid_looks_like_toki(pid) {
                // Stale PID file (dead, or PID reused by another process) — clean up
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
    // Also confirm the live PID is actually toki, not a reused PID.
    if alive && pid_looks_like_toki(pid) { Some(pid) } else { None }
}

#[cfg(test)]
mod pid_guard_tests {
    use super::pid_looks_like_toki;

    #[test]
    fn current_test_process_is_toki() {
        // The test binary is named `toki-<hash>`, so its comm contains "toki".
        assert!(pid_looks_like_toki(std::process::id()));
    }

    #[test]
    fn pid1_is_not_toki() {
        // PID 1 (launchd/init) is always alive and never toki.
        assert!(!pid_looks_like_toki(1));
    }
}
