/// Default Claude Code root on macOS: ~/.claude
#[allow(dead_code)]
pub fn default_claude_root() -> String {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".claude").to_string_lossy().to_string()
}

const PLIST_LABEL: &str = "com.toki.daemon";

fn plist_path() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join("Library/LaunchAgents").join(format!("{}.plist", PLIST_LABEL))
}

fn toki_binary_path() -> String {
    std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("toki"))
        .to_string_lossy()
        .to_string()
}

/// Install and load a LaunchAgent plist for auto-start on login.
pub fn enable_autostart() -> Result<(), String> {
    let path = plist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let binary = toki_binary_path();
    let plist = format!(
r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>daemon</string>
        <string>start</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>"#, PLIST_LABEL, binary);

    std::fs::write(&path, &plist).map_err(|e| e.to_string())?;

    // Load the agent
    let status = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&path)
        .status()
        .map_err(|e| e.to_string())?;

    if !status.success() {
        return Err("launchctl load failed".to_string());
    }

    Ok(())
}

/// Unload and remove the LaunchAgent plist.
pub fn disable_autostart() -> Result<(), String> {
    let path = plist_path();
    if !path.exists() {
        return Ok(());
    }

    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&path)
        .status();

    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Check if the LaunchAgent plist exists.
pub fn is_autostart_enabled() -> bool {
    plist_path().exists()
}
