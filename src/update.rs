use std::path::{Path, PathBuf};

const GITHUB_LATEST_URL: &str = "https://api.github.com/repos/korjwl1/toki/releases/latest";
/// Only check once per this interval (seconds).
const CHECK_INTERVAL_SECS: u64 = 86400; // 24 hours

#[derive(serde::Serialize, serde::Deserialize)]
struct UpdateCache {
    latest_version: String,
    checked_at: u64,
}

/// Default cache file path.
pub fn default_cache_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("toki").join("update_check.json")
}

/// Check for updates. Returns Some("x.y.z") if a newer version is available.
/// Caches the result for CHECK_INTERVAL_SECS to avoid hitting GitHub on every invocation.
pub fn check_for_update(cache_path: &Path) -> Option<String> {
    let current = env!("CARGO_PKG_VERSION");
    let latest = get_latest_version(cache_path)?;

    if version_newer(&latest, current) {
        Some(latest)
    } else {
        None
    }
}

fn get_latest_version(cache_path: &Path) -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    // Check cache first
    if let Some(cached) = load_cache(cache_path) {
        if now - cached.checked_at < CHECK_INTERVAL_SECS {
            return Some(cached.latest_version);
        }
    }

    // Fetch from GitHub (non-blocking timeout)
    let resp = ureq::get(GITHUB_LATEST_URL)
        .set("Accept", "application/vnd.github.v3+json")
        .set("User-Agent", "toki-update-check")
        .timeout(std::time::Duration::from_secs(3))
        .call()
        .ok()?;

    let body_str = resp.into_string().ok()?;
    let body: serde_json::Value = serde_json::from_str(&body_str).ok()?;
    let tag = body["tag_name"].as_str()?;
    let version = tag.strip_prefix('v').unwrap_or(tag).to_string();

    // Save cache
    let cache = UpdateCache {
        latest_version: version.clone(),
        checked_at: now,
    };
    save_cache(cache_path, &cache);

    Some(version)
}

fn load_cache(path: &Path) -> Option<UpdateCache> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_cache(path: &Path, cache: &UpdateCache) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, json);
    }
}

/// Compare semver strings. Returns true if `latest` is newer than `current`.
fn version_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> (u32, u32, u32) {
        let parts: Vec<&str> = v.split('.').collect();
        let major = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch = parts.get(2).and_then(|s| s.split('-').next()?.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(latest) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_newer() {
        assert!(version_newer("1.2.0", "1.1.5"));
        assert!(version_newer("1.1.6", "1.1.5"));
        assert!(version_newer("2.0.0", "1.9.9"));
        assert!(!version_newer("1.1.5", "1.1.5"));
        assert!(!version_newer("1.1.4", "1.1.5"));
        assert!(!version_newer("1.1.5-alpha", "1.1.5"));
    }
}
