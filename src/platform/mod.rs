#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;

/// Enable auto-start on login (platform-specific).
pub fn enable_autostart() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    { return macos::enable_autostart(); }
    #[cfg(target_os = "linux")]
    { return linux::enable_autostart(); }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    { Err("auto-start not supported on this platform".to_string()) }
}

/// Disable auto-start on login (platform-specific).
pub fn disable_autostart() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    { return macos::disable_autostart(); }
    #[cfg(target_os = "linux")]
    { return linux::disable_autostart(); }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    { Err("auto-start not supported on this platform".to_string()) }
}

/// Check if auto-start is enabled (platform-specific).
pub fn is_autostart_enabled() -> bool {
    #[cfg(target_os = "macos")]
    { return macos::is_autostart_enabled(); }
    #[cfg(target_os = "linux")]
    { return linux::is_autostart_enabled(); }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    { false }
}

/// Stable wrapper symlinks a package manager keeps pointing at the current
/// install across upgrades. Checked in order.
const STABLE_BINARY_SYMLINKS: &[&str] = &[
    "/opt/homebrew/bin/toki",              // Homebrew on Apple Silicon
    "/usr/local/bin/toki",                 // Homebrew on Intel / manual installs
    "/home/linuxbrew/.linuxbrew/bin/toki", // Linuxbrew
];

/// Resolve a stable path to the toki binary for baking into autostart units.
///
/// `std::env::current_exe()` resolves symlinks, so under Homebrew it returns the
/// version-pinned Cellar path (e.g. `/opt/homebrew/Cellar/toki/2.1.0/bin/toki`).
/// Writing that into a LaunchAgent/systemd unit breaks autostart on the next
/// `brew upgrade`, when the old Cellar directory is deleted. Prefer a stable
/// wrapper symlink that resolves to the same binary we're running, so the unit
/// keeps working across upgrades. Falls back to the resolved path (correct for
/// non-package-manager installs, e.g. `cargo install`).
pub fn stable_binary_path() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("toki"));
    resolve_stable_path(&exe, STABLE_BINARY_SYMLINKS)
}

fn resolve_stable_path(exe: &Path, candidates: &[&str]) -> String {
    let exe_canon = std::fs::canonicalize(exe).ok();
    if exe_canon.is_some() {
        for cand in candidates {
            if let Ok(resolved) = std::fs::canonicalize(cand) {
                if exe_canon.as_ref() == Some(&resolved) {
                    return (*cand).to_string();
                }
            }
        }
    }
    exe.to_string_lossy().to_string()
}

#[cfg(test)]
mod stable_path_tests {
    use super::resolve_stable_path;

    #[test]
    fn prefers_symlink_that_points_at_current_exe() {
        let dir = std::env::temp_dir().join(format!("toki-stable-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let real = dir.join("toki-real");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        let link = dir.join("toki-link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let link_str = link.to_string_lossy().to_string();
        // exe == resolved real binary, candidate == stable symlink → pick the symlink
        assert_eq!(resolve_stable_path(&real, &[&link_str]), link_str);
        // no candidate matches → fall back to the exe path as passed (not canonicalized)
        assert_eq!(
            resolve_stable_path(&real, &["/nonexistent/toki"]),
            real.to_string_lossy().to_string()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

use crossbeam_channel::Sender;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;

/// Create a file watcher that sends changed file paths over a crossbeam channel.
pub fn create_watcher(
    tx: Sender<String>,
) -> notify::Result<RecommendedWatcher> {
    let debug = std::env::var("TOKI_DEBUG").map_or(false, |v| v == "1" || v == "2" || v == "true");
    let watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match &res {
            Ok(event) => {
                if debug {
                    eprintln!("[toki:watcher] event: kind={:?} paths={:?}", event.kind, event.paths);
                }
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        for path in &event.paths {
                            if let Some(path_str) = path.to_str() {
                                if path_str.ends_with(".jsonl") {
                                    let _ = tx.send(path_str.to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Err(e) => {
                if debug {
                    eprintln!("[toki:watcher] error: {:?}", e);
                }
            }
        }
    })?;

    Ok(watcher)
}

/// Register a directory for recursive watching.
pub fn watch_directory(
    watcher: &mut RecommendedWatcher,
    dir: &str,
) -> notify::Result<()> {
    let path = Path::new(dir);
    if path.exists() {
        watcher.watch(path, RecursiveMode::Recursive)?;
    }
    Ok(())
}
