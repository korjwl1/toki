#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;

use crossbeam_channel::Sender;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;

/// Create a file watcher that sends changed file paths over a crossbeam channel.
pub fn create_watcher(
    tx: Sender<String>,
) -> notify::Result<RecommendedWatcher> {
    let watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    for path in event.paths {
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
