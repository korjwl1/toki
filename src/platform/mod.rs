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
