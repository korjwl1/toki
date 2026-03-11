use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clitrace::Config;

// Global flag for signal handler
static RUNNING: std::sync::atomic::AtomicBool = AtomicBool::new(true);

fn main() {
    let mut config = Config::new();

    if let Ok(root) = std::env::var("CLITRACE_CLAUDE_ROOT") {
        config = config.with_claude_root(root);
    }
    if let Ok(db_path) = std::env::var("CLITRACE_DB_PATH") {
        config = config.with_db_path(db_path.into());
    }

    println!("[clitrace] Starting...");
    println!("[clitrace] Claude Code root: {}", config.claude_code_root);
    println!("[clitrace] Database: {}", config.db_path.display());

    let handle = match clitrace::start(config) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[clitrace] Failed to start: {}", e);
            std::process::exit(1);
        }
    };

    println!("[clitrace] Listening for file changes... (Ctrl+C to stop)");

    // Register SIGINT handler
    unsafe {
        libc::signal(libc::SIGINT, sigint_handler as libc::sighandler_t);
    }

    // Wait until SIGINT
    while RUNNING.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    println!("\n[clitrace] Shutting down...");
    handle.stop();
    println!("[clitrace] Done.");
}

extern "C" fn sigint_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}
