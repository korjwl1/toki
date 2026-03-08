/// Default Claude Code root on macOS: ~/.claude
pub fn default_claude_root() -> String {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".claude").to_string_lossy().to_string()
}
