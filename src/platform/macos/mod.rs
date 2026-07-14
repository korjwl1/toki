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
    // Prefer a stable package-manager symlink over the version-pinned Cellar
    // path so the LaunchAgent survives `brew upgrade` (see platform::stable_binary_path).
    super::stable_binary_path()
}

/// Install and load a LaunchAgent plist for auto-start on login.
pub fn enable_autostart() -> Result<(), String> {
    let path = plist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let binary = toki_binary_path();
    // Run the daemon in the foreground so launchd supervises the real process.
    // Plain `daemon start` double-spawns a detached child and exits, leaving
    // launchd watching a process that is already gone — the launchd anti-pattern.
    // KeepAlive is restart-on-crash-only: a clean `toki daemon stop` exits 0 and
    // stays stopped, while a crash (non-zero) is relaunched.
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
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
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

/// When the LaunchAgent is installed, (re)start the daemon through launchd
/// instead of spawning a detached process.
///
/// A plain `daemon start` double-spawns a detached child that launchd does not
/// supervise, while the loaded launchd job sits stopped — and because a clean
/// `daemon stop` exits 0, KeepAlive/SuccessfulExit never relaunches it, so
/// crash-restart is dead. `launchctl kickstart` starts the job under launchd;
/// `-k` force-restarts a running one (used for `daemon restart`).
///
/// Returns `None` when autostart is not enabled, so the caller falls back to the
/// detached spawn; otherwise `Some` with the launchctl outcome.
pub fn supervised_kickstart(force_restart: bool) -> Option<Result<(), String>> {
    if !is_autostart_enabled() {
        return None;
    }
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{}/{}", uid, PLIST_LABEL);
    let mut args: Vec<&str> = vec!["kickstart"];
    if force_restart {
        args.push("-k");
    }
    args.push(&target);
    let result = match std::process::Command::new("launchctl").args(&args).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("launchctl kickstart failed ({})", s)),
        Err(e) => Err(format!("failed to run launchctl: {}", e)),
    };
    Some(result)
}
