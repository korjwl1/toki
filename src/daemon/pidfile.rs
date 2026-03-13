use std::path::{Path, PathBuf};

/// Default PID file path.
pub fn default_pidfile_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("clitrace").join("daemon.pid")
}

/// Write the current process PID to file.
pub fn write_pidfile(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let pid = std::process::id();
    std::fs::write(path, pid.to_string()).ok();
}

/// Read PID from file. Returns None if file doesn't exist or is invalid.
pub fn read_pidfile(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Remove PID file.
pub fn remove_pidfile(path: &Path) {
    std::fs::remove_file(path).ok();
}
