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
    let home = crate::config::home_dir();
    home.join(".config").join("toki").join("update_check.json")
}

/// Check for updates, refreshing the cache from GitHub if stale (may block on
/// the network up to ~3s). Intended for the long-lived daemon — NOT for
/// short-lived CLI commands, which should use `cached_update` instead so they
/// never hang on the network. Returns Some("x.y.z") if a newer version exists.
pub fn check_for_update(cache_path: &Path) -> Option<String> {
    let current = env!("CARGO_PKG_VERSION");
    let latest = get_latest_version(cache_path)?;

    if version_newer(&latest, current) {
        Some(latest)
    } else {
        None
    }
}

/// Read the cached latest version WITHOUT any network access. Returns Some(version)
/// if the cached latest is newer than the running binary. Used by CLI commands so
/// printing an update hint never blocks; the daemon keeps the cache fresh via
/// `check_for_update` (see `run_daemon_foreground`).
pub fn cached_update(cache_path: &Path) -> Option<String> {
    let current = env!("CARGO_PKG_VERSION");
    let cached = load_cache(cache_path)?;
    if version_newer(&cached.latest_version, current) {
        Some(cached.latest_version)
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
    let cached = load_cache(cache_path);
    if let Some(ref c) = cached {
        if now - c.checked_at < CHECK_INTERVAL_SECS {
            return Some(c.latest_version.clone());
        }
    }

    // Fetch from GitHub (non-blocking timeout)
    let resp = match ureq::get(GITHUB_LATEST_URL)
        .set("Accept", "application/vnd.github.v3+json")
        .set("User-Agent", "toki-update-check")
        .timeout(std::time::Duration::from_secs(3))
        .call()
    {
        Ok(r) => r,
        // Unauthenticated GitHub API is limited to 60 req/hr. When throttled it
        // answers 403 (or 429); surface a warning instead of silently reporting
        // "no update", and fall back to the last cached value if we have one.
        Err(ureq::Error::Status(code, _)) if code == 403 || code == 429 => {
            eprintln!(
                "[toki] update check: GitHub API rate limit hit (HTTP {code}); \
                       keeping last known version"
            );
            return cached.map(|c| c.latest_version);
        }
        Err(_) => return cached.map(|c| c.latest_version),
    };

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
    let Ok(json) = serde_json::to_string(cache) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic write (tmp + rename) so a concurrent reader (CLI) never sees a
    // half-written file when the daemon refreshes the cache in the background.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Compare semver strings. Returns true if `latest` is newer than `current`.
///
/// Prereleases are never offered as updates: a `latest` carrying a prerelease
/// suffix (e.g. "1.3.0-rc.1") returns false, so it counts as not-newer than the
/// stable it derives from. This matches the Swift monitor, which explicitly
/// excludes prereleases.
fn version_newer(latest: &str, current: &str) -> bool {
    if latest.contains('-') {
        return false;
    }
    let parse = |v: &str| -> (u32, u32, u32) {
        let parts: Vec<&str> = v.split('.').collect();
        let major = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch = parts
            .get(2)
            .and_then(|s| s.split('-').next()?.parse().ok())
            .unwrap_or(0);
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

    #[test]
    fn test_version_newer_ignores_prereleases() {
        // A prerelease is never offered as an update, even with a higher core.
        assert!(!version_newer("1.2.0-beta", "1.1.5"));
        assert!(!version_newer("2.0.0-rc.1", "1.0.0"));
        assert!(!version_newer("1.2.0-alpha.2", "1.2.0"));
        // Stable releases still compare normally, even past a prerelease current.
        assert!(version_newer("1.1.6", "1.1.5-alpha"));
        assert!(version_newer("1.2.0", "1.1.5"));
    }

    #[test]
    fn test_cached_update_reads_cache_only() {
        let dir = std::env::temp_dir().join(format!("toki-upd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("update_check.json");

        // A far-future version in cache → reported as available, no network.
        save_cache(
            &path,
            &UpdateCache {
                latest_version: "999.0.0".into(),
                checked_at: 0,
            },
        );
        assert_eq!(cached_update(&path).as_deref(), Some("999.0.0"));

        // An old version → no update.
        save_cache(
            &path,
            &UpdateCache {
                latest_version: "0.0.1".into(),
                checked_at: 0,
            },
        );
        assert_eq!(cached_update(&path), None);

        // Missing cache → no update (and never panics / never hits network).
        let _ = std::fs::remove_file(&path);
        assert_eq!(cached_update(&path), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_cache_creates_parent_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("toki-upd-mk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Parent does not exist yet — save_cache must create it.
        let path = dir.join("nested").join("update_check.json");
        save_cache(
            &path,
            &UpdateCache {
                latest_version: "1.2.3".into(),
                checked_at: 42,
            },
        );
        let loaded = load_cache(&path).expect("cache should round-trip");
        assert_eq!(loaded.latest_version, "1.2.3");
        assert_eq!(loaded.checked_at, 42);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
