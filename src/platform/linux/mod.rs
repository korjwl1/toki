use std::path::PathBuf;

const SERVICE_NAME: &str = "toki.service";

fn service_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config/systemd/user").join(SERVICE_NAME)
}

fn toki_binary_path() -> String {
    std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("toki"))
        .to_string_lossy()
        .to_string()
}

/// Install and enable a systemd user service for auto-start on login.
pub fn enable_autostart() -> Result<(), String> {
    let path = service_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let binary = toki_binary_path();
    let unit = format!(
"[Unit]\n\
Description=toki token usage tracker daemon\n\
After=default.target\n\
\n\
[Service]\n\
Type=forking\n\
ExecStart={} daemon start\n\
ExecStop={} daemon stop\n\
Restart=on-failure\n\
RestartSec=5\n\
\n\
[Install]\n\
WantedBy=default.target\n", binary, binary);

    std::fs::write(&path, &unit).map_err(|e| e.to_string())?;

    let status = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("systemctl daemon-reload failed".to_string());
    }

    let status = std::process::Command::new("systemctl")
        .args(["--user", "enable", SERVICE_NAME])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("systemctl enable failed".to_string());
    }

    Ok(())
}

/// Disable and remove the systemd user service.
pub fn disable_autostart() -> Result<(), String> {
    let path = service_path();
    if !path.exists() {
        return Ok(());
    }

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", SERVICE_NAME])
        .status();

    std::fs::remove_file(&path).map_err(|e| e.to_string())?;

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    Ok(())
}

/// Check if the systemd user service file exists.
pub fn is_autostart_enabled() -> bool {
    service_path().exists()
}
