/// Secure credential storage for toki sync.
///
/// macOS: Keychain (via `keyring` crate → Security.framework)
/// Linux: ~/.config/toki/sync.json with chmod 600
///
/// Stored JSON: { "server_addr", "http_url", "access_token", "refresh_token" }

use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "toki-sync";
const KEYRING_USER: &str = "credentials";

/// Sync credentials stored securely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// TCP sync address (host:port), e.g. "sync.example.com:9090"
    pub server_addr: String,
    /// HTTP base URL for API calls, e.g. "https://sync.example.com"
    pub http_url: String,
    /// Current access token (JWT)
    pub access_token: String,
    /// Refresh token for obtaining new access tokens
    pub refresh_token: String,
    /// Stable UUID that uniquely identifies this device across reconnects.
    /// Generated once at `toki sync enable` and stored permanently.
    #[serde(default)]
    pub device_key: String,
    /// Human-readable device name (typically hostname).
    #[serde(default)]
    pub device_name: String,
}

/// Save credentials to secure storage.
pub fn save(creds: &Credentials) -> Result<(), String> {
    let json = serde_json::to_string(creds).map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .map_err(|e| format!("keychain entry: {e}"))?;
        entry.set_password(&json).map_err(|e| format!("keychain write: {e}"))?;
        return Ok(());
    }

    #[cfg(not(target_os = "macos"))]
    {
        save_to_file(&json)
    }
}

/// Load credentials from secure storage. Returns None if not configured.
pub fn load() -> Option<Credentials> {
    #[cfg(target_os = "macos")]
    {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
        let json = entry.get_password().ok()?;
        serde_json::from_str(&json).ok()
    }

    #[cfg(not(target_os = "macos"))]
    {
        let json = load_from_file()?;
        serde_json::from_str(&json).ok()
    }
}

/// Delete stored credentials.
pub fn delete() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .map_err(|e| format!("keychain entry: {e}"))?;
        match entry.delete_password() {
            Ok(()) => return Ok(()),
            Err(keyring::Error::NoEntry) => return Ok(()),
            Err(e) => return Err(format!("keychain delete: {e}")),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        delete_file()
    }
}

/// Check and warn if the credentials file has overly permissive permissions (Linux only).
pub fn check_file_permissions() {
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(path) = creds_file_path() {
            if path.exists() {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&path) {
                    let mode = meta.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        eprintln!(
                            "[toki] WARNING: sync credentials file {} has permissions {:o} (expected 600). \
                             Run: chmod 600 {}",
                            path.display(), mode, path.display()
                        );
                    }
                }
            }
        }
    }
}

// ─── File-based storage (non-macOS) ──────────────────────────────────────────

#[cfg(not(target_os = "macos"))]
fn creds_file_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("toki").join("sync.json"))
}

#[cfg(not(target_os = "macos"))]
fn save_to_file(json: &str) -> Result<(), String> {
    let path = creds_file_path().ok_or("cannot determine config dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }

    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("open credentials: {e}"))?;
        file.write_all(json.as_bytes()).map_err(|e| format!("write credentials: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, json).map_err(|e| format!("write credentials: {e}"))?;
    }

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn load_from_file() -> Option<String> {
    let path = creds_file_path()?;
    std::fs::read_to_string(path).ok()
}

#[cfg(not(target_os = "macos"))]
fn delete_file() -> Result<(), String> {
    let path = creds_file_path().ok_or("cannot determine config dir")?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("delete credentials: {e}"))?;
    }
    Ok(())
}
