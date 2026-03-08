use webtrace::Config;

fn main() {
    let mut config = Config::new();

    if let Ok(root) = std::env::var("WEBTRACE_CLAUDE_ROOT") {
        config = config.with_claude_root(root);
    }
    if let Ok(db_path) = std::env::var("WEBTRACE_DB_PATH") {
        config = config.with_db_path(db_path.into());
    }

    println!("[webtrace] Starting...");
    println!("[webtrace] Claude Code root: {}", config.claude_code_root);
    println!("[webtrace] Database: {}", config.db_path.display());

    // TODO: Phase 4 - start(config) -> Handle, Ctrl+C signal handling
    println!("[webtrace] Not yet implemented. See Phase 4.");
}
