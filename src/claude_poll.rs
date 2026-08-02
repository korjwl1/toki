//! Claude rate-limit window poller.
//!
//! Codex windows are extracted passively from rollout files; Claude exposes
//! window state only behind `api.anthropic.com/api/oauth/usage`, so the daemon
//! polls it actively — but only while tokens are actually flowing (the poller
//! is woken by the writer's flush, so it fires exclusively when Claude Code
//! itself is making API calls), plus one reserved confirm sample just before a
//! known reset. Sleeping machines cost zero.
//!
//! Scheduling is a single-flight, per-account budget with a hard 30s floor:
//! UI-triggered refreshes (`max_age=0`) request freshness, they never bypass
//! the call budget. Exception: when the previous poll failed on auth (or no
//! data exists yet), a token-flow wake polls immediately — a live token stream
//! implies re-authentication, and this preserves the monitor's 1–2s re-login
//! recovery with daemon-sourced data.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::common::types::WindowObservation;
use crate::db::Database;
use crate::windows::WindowTracker;
use crate::writer::DbOp;

/// Hard floor between provider calls; nothing bypasses it in a healthy state.
const POLL_FLOOR_MS: i64 = 30_000;
/// Steady-state interval while active (community convention for this endpoint
/// is >= 180s).
const POLL_STEADY_MS: i64 = 180_000;
/// Densified interval above 90% utilization (time_to_100 precision).
const POLL_DENSE_MS: i64 = 30_000;
/// Reserved confirm sample offset before a known reset.
const CONFIRM_BEFORE_RESET_MS: i64 = 120_000;
/// "Active" means a token-flow wake within this horizon.
const ACTIVE_HORIZON_MS: i64 = 30 * 60_000;
/// Re-resolve the account/plan via the profile endpoint at most this often.
const PROFILE_REFRESH_MS: i64 = 24 * 3_600_000;
/// Backoff after a 429 from the usage endpoint.
const RATE_LIMITED_BACKOFF_MS: i64 = 300_000;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// The endpoint aggressively rate-limits unknown user agents; tools that poll
/// it identify as claude-code (see plan §4). Version is nominal.
const USER_AGENT: &str = "claude-code/2.1.220 (external, toki)";
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Auth state as classified by the daemon, mirroring the monitor's
/// ClaudeAuthResult so the UI's login-detection flows keep working unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    Ok,
    Missing,
    Expired,
    Unreadable,
}

impl AuthStatus {
    pub fn label(&self) -> &'static str {
        match self {
            AuthStatus::Ok => "ok",
            AuthStatus::Missing => "missing",
            AuthStatus::Expired => "expired",
            AuthStatus::Unreadable => "unreadable",
        }
    }
}

#[derive(Default)]
struct Signal {
    token_flow: bool,
    refresh: bool,
    stop: bool,
}

/// Published state consumed by the UDS `WINDOWS` handler.
#[derive(Clone)]
pub struct PublishedState {
    pub auth_status: AuthStatus,
    pub last_success_ms: i64,
    pub last_poll_ms: i64,
    pub plan: String,
    /// Claude extra-usage (pay-per-overflow) enabled on the account.
    pub extra_usage_enabled: bool,
    /// Refresh bookkeeping: a WINDOWS request with max_age=0 bumps want_seq;
    /// the poller bumps done_seq after the next completed poll attempt.
    want_seq: u64,
    done_seq: u64,
}

impl Default for PublishedState {
    fn default() -> Self {
        PublishedState {
            auth_status: AuthStatus::Missing,
            last_success_ms: 0,
            last_poll_ms: 0,
            plan: String::new(),
            extra_usage_enabled: false,
            want_seq: 0,
            done_seq: 0,
        }
    }
}

/// Shared hub between the poller thread, the writer (token-flow wake), the UDS
/// listener (refresh + state reads), and shutdown.
pub struct PollerHub {
    signal: (Mutex<Signal>, Condvar),
    state: (Mutex<PublishedState>, Condvar),
    /// Last token-flow wake, for the active-horizon gate. Written by the writer
    /// thread on every flush — atomic, never locks the writer.
    last_token_flow_ms: AtomicI64,
    /// Provider roots for auth classification in the WINDOWS handler.
    pub claude_root: Option<String>,
    pub codex_root: Option<String>,
    /// Whether active Claude polling is currently enabled (window_polling
    /// setting; hot-reloadable via the settings watcher).
    polling_enabled: std::sync::atomic::AtomicBool,
}

impl PollerHub {
    pub fn new(claude_root: Option<String>, codex_root: Option<String>, polling_enabled: bool) -> Self {
        PollerHub {
            signal: (Mutex::new(Signal::default()), Condvar::new()),
            state: (Mutex::new(PublishedState::default()), Condvar::new()),
            last_token_flow_ms: AtomicI64::new(0),
            claude_root,
            codex_root,
            polling_enabled: std::sync::atomic::AtomicBool::new(polling_enabled),
        }
    }

    pub fn polling_enabled(&self) -> bool {
        self.polling_enabled.load(Ordering::Relaxed)
    }

    /// Hot-reload hook: flips active polling without a daemon restart. The
    /// poller thread checks this before each provider call; the passive
    /// (window_tracking) pipeline is engine-embedded and stays restart-bound.
    pub fn set_polling_enabled(&self, enabled: bool) {
        self.polling_enabled.store(enabled, Ordering::Relaxed);
        let (_, cvar) = &self.signal;
        cvar.notify_all();
    }

    /// Called by the writer after each flush: tokens are flowing.
    pub fn notify_token_flow(&self) {
        self.last_token_flow_ms.store(now_ms(), Ordering::Relaxed);
        let (lock, cvar) = &self.signal;
        if let Ok(mut s) = lock.lock() {
            s.token_flow = true;
            cvar.notify_all();
        }
    }

    pub fn stop(&self) {
        let (lock, cvar) = &self.signal;
        if let Ok(mut s) = lock.lock() {
            s.stop = true;
            cvar.notify_all();
        }
    }

    pub fn state_snapshot(&self) -> PublishedState {
        self.state.0.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Request a fresh poll and wait (bounded) for the poller to complete one
    /// attempt. Returns the resulting state; on timeout the caller serves stale
    /// data marked `refreshing: true`.
    pub fn request_refresh(&self, wait: Duration) -> (PublishedState, bool) {
        let want = {
            let mut st = match self.state.0.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            st.want_seq += 1;
            st.want_seq
        };
        {
            let (lock, cvar) = &self.signal;
            if let Ok(mut s) = lock.lock() {
                s.refresh = true;
                cvar.notify_all();
            }
        }
        let (lock, cvar) = &self.state;
        let guard = match lock.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let (st, timeout) = cvar
            .wait_timeout_while(guard, wait, |st| st.done_seq < want)
            .map(|(g, t)| (g.clone(), t.timed_out()))
            .unwrap_or_else(|e| {
                let g = e.into_inner();
                (g.0.clone(), true)
            });
        (st, timeout)
    }

    fn publish<F: FnOnce(&mut PublishedState)>(&self, f: F) {
        let (lock, cvar) = &self.state;
        if let Ok(mut st) = lock.lock() {
            f(&mut st);
            cvar.notify_all();
        }
    }

    fn last_token_flow(&self) -> i64 {
        self.last_token_flow_ms.load(Ordering::Relaxed)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- Credentials ----

pub struct ClaudeCredentials {
    pub access_token: String,
    pub expires_at_ms: i64,
}

/// Read Claude Code's OAuth credentials without ever refreshing them (Claude
/// Code owns the refresh cycle; re-read on every poll instead of caching).
/// macOS: Keychain via the `security` CLI — service-only lookup, which the
/// keyring crate cannot express (it requires the account name, which varies).
/// Elsewhere: `<claude_root>/.credentials.json`, same JSON shape.
pub fn read_claude_credentials(claude_root: &str) -> Result<ClaudeCredentials, AuthStatus> {
    let raw = read_credentials_raw(claude_root)?;
    parse_credentials(&raw)
}

fn read_credentials_raw(claude_root: &str) -> Result<String, AuthStatus> {
    #[cfg(target_os = "macos")]
    {
        let _ = claude_root;
        let out = std::process::Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .output()
            .map_err(|_| AuthStatus::Unreadable)?;
        if !out.status.success() {
            // Parity with the monitor's ClaudeAuthReader.classify: exit 44 is
            // errSecItemNotFound (logged out); any other failure — ACL denial,
            // keychain locked, signal — is transient and must NOT be treated
            // as "missing" (that would wipe valid UI state).
            return Err(if out.status.code() == Some(44) {
                AuthStatus::Missing
            } else {
                AuthStatus::Unreadable
            });
        }
        String::from_utf8(out.stdout).map_err(|_| AuthStatus::Unreadable)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let path = std::path::Path::new(claude_root).join(".credentials.json");
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AuthStatus::Missing),
            Err(_) => Err(AuthStatus::Unreadable),
        }
    }
}

fn parse_credentials(raw: &str) -> Result<ClaudeCredentials, AuthStatus> {
    #[derive(Deserialize)]
    struct Root {
        #[serde(rename = "claudeAiOauth")]
        oauth: Option<Oauth>,
    }
    #[derive(Deserialize)]
    struct Oauth {
        #[serde(rename = "accessToken")]
        access_token: Option<String>,
        #[serde(rename = "expiresAt")]
        expires_at: Option<i64>,
    }
    let root: Root = serde_json::from_str(raw.trim()).map_err(|_| AuthStatus::Unreadable)?;
    let oauth = root.oauth.ok_or(AuthStatus::Missing)?;
    let access_token = match oauth.access_token {
        Some(t) if !t.is_empty() => t,
        _ => return Err(AuthStatus::Missing),
    };
    let expires_at_ms = oauth.expires_at.unwrap_or(0);
    if expires_at_ms > 0 && expires_at_ms < now_ms() {
        return Err(AuthStatus::Expired);
    }
    Ok(ClaudeCredentials { access_token, expires_at_ms })
}

/// Codex auth classification for the WINDOWS handler: read on demand — always
/// current, no inode watcher needed (the monitor's watcher existed to trigger
/// its own HTTP re-polls; the daemon reads the file per request instead).
pub fn codex_auth_status(codex_root: &str) -> AuthStatus {
    let path = std::path::Path::new(codex_root).join("auth.json");
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) if v.get("tokens").map(|t| !t.is_null()).unwrap_or(false) => AuthStatus::Ok,
            Ok(_) => AuthStatus::Missing,
            Err(_) => AuthStatus::Unreadable,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AuthStatus::Missing,
        Err(_) => AuthStatus::Unreadable,
    }
}

// ---- Usage / profile clients ----

#[derive(Deserialize)]
struct UsageBucketRaw {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct ExtraUsageRaw {
    is_enabled: Option<bool>,
}

#[derive(Deserialize)]
struct UsageResponseRaw {
    five_hour: Option<UsageBucketRaw>,
    seven_day: Option<UsageBucketRaw>,
    seven_day_sonnet: Option<UsageBucketRaw>,
    seven_day_opus: Option<UsageBucketRaw>,
    extra_usage: Option<ExtraUsageRaw>,
}

enum PollError {
    AuthRejected,
    RateLimited,
    Other(String),
}

fn fetch_usage(token: &str) -> Result<UsageResponseRaw, PollError> {
    let resp = ureq::get(USAGE_URL)
        .set("Authorization", &format!("Bearer {}", token))
        .set("anthropic-beta", OAUTH_BETA)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(10))
        .call();
    match resp {
        Ok(r) => r
            .into_json::<UsageResponseRaw>()
            .map_err(|e| PollError::Other(format!("usage parse: {}", e))),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            Err(PollError::AuthRejected)
        }
        Err(ureq::Error::Status(429, _)) => Err(PollError::RateLimited),
        Err(e) => Err(PollError::Other(e.to_string())),
    }
}

struct ProfileInfo {
    account_scope: String,
    plan: String,
}

fn fetch_profile(token: &str) -> Option<ProfileInfo> {
    #[derive(Deserialize)]
    struct ProfileRaw {
        account: Option<AccountRaw>,
        organization: Option<OrgRaw>,
    }
    #[derive(Deserialize)]
    struct AccountRaw {
        uuid: Option<String>,
    }
    #[derive(Deserialize)]
    struct OrgRaw {
        rate_limit_tier: Option<String>,
    }
    let resp = ureq::get(PROFILE_URL)
        .set("Authorization", &format!("Bearer {}", token))
        .set("anthropic-beta", OAUTH_BETA)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?;
    let p: ProfileRaw = resp.into_json().ok()?;
    let account_scope = p
        .account
        .and_then(|a| a.uuid)
        .filter(|u| !u.is_empty())
        .map(|u| format!("{:016x}", crate::windows::hash_str(&u)))
        .unwrap_or_else(|| "default".to_string());
    let plan = p
        .organization
        .and_then(|o| o.rate_limit_tier)
        .unwrap_or_default();
    Some(ProfileInfo { account_scope, plan })
}

fn parse_iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn observations_from_usage(
    usage: &UsageResponseRaw,
    plan: &str,
    ts_ms: i64,
) -> Vec<WindowObservation> {
    let has_credits = usage
        .extra_usage
        .as_ref()
        .and_then(|e| e.is_enabled)
        .unwrap_or(false);
    let mut out = Vec::with_capacity(4);
    let mut push = |bucket: &Option<UsageBucketRaw>, limit_id: &str, window_minutes: u32| {
        if let Some(b) = bucket {
            if let (Some(pct), Some(reset_str)) = (b.utilization, b.resets_at.as_deref()) {
                if let Some(resets_at_ms) = parse_iso_ms(reset_str) {
                    out.push(WindowObservation {
                        limit_id: limit_id.to_string(),
                        window_minutes,
                        used_percent: pct,
                        resets_at_ms,
                        plan_type: if plan.is_empty() { None } else { Some(plan.to_string()) },
                        limit_reached: pct >= 99.995,
                        has_credits,
                        // The usage endpoint only serves resets_at for real
                        // windows; a 0% weekly period is genuine zero use and
                        // must exist for the overall mean.
                        anchor_stable: true,
                        ts_ms,
                    });
                }
            }
        }
    };
    push(&usage.five_hour, "five_hour", 300);
    push(&usage.seven_day, "seven_day", 10_080);
    push(&usage.seven_day_sonnet, "seven_day_sonnet", 10_080);
    push(&usage.seven_day_opus, "seven_day_opus", 10_080);
    out
}

// ---- Poller loop ----

pub fn run_claude_poller(
    hub: Arc<PollerHub>,
    _db: Arc<Database>,
    db_tx: crossbeam_channel::Sender<DbOp>,
    claude_root: String,
) {
    let mut tracker = WindowTracker::new();
    let mut last_poll_ms: i64 = 0;
    let mut consecutive_failures: u32 = 0;
    let mut backoff_until_ms: i64 = 0;
    let mut last_auth_ok = false;
    let mut peak_hint: f64 = 0.0;
    let mut profile: Option<ProfileInfo> = None;
    let mut profile_fetched_ms: i64 = 0;
    // Reset instants (minute-floored) whose pre-reset confirm sample was taken.
    let mut confirmed: std::collections::HashSet<i64> = std::collections::HashSet::new();

    loop {
        let now = now_ms();

        // Next hard deadline: the earliest pending pre-reset confirm sample.
        let next_confirm = tracker
            .open_reset_times()
            .into_iter()
            .filter(|r| !confirmed.contains(&crate::windows::floor_to_minute(*r)))
            .map(|r| r - CONFIRM_BEFORE_RESET_MS)
            .min();

        let active = now - hub.last_token_flow() < ACTIVE_HORIZON_MS;
        let interval = if peak_hint > 90.0 { POLL_DENSE_MS } else { POLL_STEADY_MS };

        // Compute how long to sleep. Idle with no pending confirm: hourly
        // housekeeping only (finalize stale windows). The scheduled deadline
        // respects an active backoff — otherwise an active session under
        // backoff would wake at the 50ms clamp in a busy loop, burning CPU.
        let mut deadline = if active {
            (last_poll_ms + interval).max(backoff_until_ms)
        } else {
            now + 3_600_000
        };
        if let Some(c) = next_confirm {
            deadline = deadline.min(c.max(now));
        }
        let wait = Duration::from_millis((deadline - now).clamp(50, 3_600_000) as u64);

        // Sleep until signal or deadline.
        let (token_flow, refresh, stop) = {
            let (lock, cvar) = &hub.signal;
            let guard = match lock.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            let (mut s, _t) = cvar
                .wait_timeout_while(guard, wait, |s| !s.token_flow && !s.refresh && !s.stop)
                .unwrap_or_else(|e| {
                    let g = e.into_inner();
                    (g.0, g.1)
                });
            let out = (s.token_flow, s.refresh, s.stop);
            s.token_flow = false;
            s.refresh = false;
            out
        };

        if stop {
            for w in tracker.flush_all() {
                let _ = db_tx.send(DbOp::WriteWindow(Box::new(w)));
            }
            break;
        }

        let now = now_ms();

        // Finalize expired windows regardless of polling decisions.
        for w in tracker.finalize_expired(now) {
            confirmed.remove(&crate::windows::floor_to_minute(w.snapshot.raw_resets_at_ms));
            let _ = db_tx.send(DbOp::WriteWindow(Box::new(w)));
        }

        // ---- Decide whether to poll now ----
        let confirm_due = next_confirm.map(|c| now >= c).unwrap_or(false);
        let active = now - hub.last_token_flow() < ACTIVE_HORIZON_MS;
        let since_last = now - last_poll_ms;

        // Immediate path: a token-flow wake while auth was failing (or before
        // any successful poll) implies re-login — bypass the floor once.
        let relogin_probe = token_flow && !last_auth_ok;

        let scheduled = active && since_last >= interval;
        let refresh_due = refresh && since_last >= POLL_FLOOR_MS;
        let floor_ok = since_last >= POLL_FLOOR_MS || relogin_probe || confirm_due;
        let backing_off = now < backoff_until_ms && !relogin_probe && !confirm_due;

        if !(scheduled || refresh_due || relogin_probe || confirm_due) || !floor_ok || backing_off {
            // A refresh request that we won't serve with a fresh poll still
            // completes (stale-is-fine contract).
            if refresh {
                hub.publish(|st| st.done_seq = st.want_seq);
            }
            continue;
        }

        // Hot-reload gate: window_polling can be flipped off at runtime.
        if !hub.polling_enabled() {
            if refresh {
                hub.publish(|st| st.done_seq = st.want_seq);
            }
            continue;
        }

        // ---- Poll ----
        last_poll_ms = now;
        let result = match read_claude_credentials(&claude_root) {
            Err(status) => Err((status, None)),
            Ok(creds) => {
                // Auth just recovered — possibly a different account. Drop the
                // cached profile BEFORE the fetch below so this same poll
                // re-resolves it (invalidating after the poll would wipe the
                // profile the first successful poll just fetched).
                if !last_auth_ok {
                    profile = None;
                }
                // Account/plan resolution piggybacks on a valid token.
                if profile.is_none() || now - profile_fetched_ms > PROFILE_REFRESH_MS {
                    if let Some(p) = fetch_profile(&creds.access_token) {
                        tracker.set_account(&p.account_scope);
                        profile = Some(p);
                        profile_fetched_ms = now;
                    }
                }
                match fetch_usage(&creds.access_token) {
                    Ok(usage) => Ok(usage),
                    Err(PollError::AuthRejected) => Err((AuthStatus::Expired, None)),
                    Err(PollError::RateLimited) => {
                        Err((AuthStatus::Ok, Some(RATE_LIMITED_BACKOFF_MS)))
                    }
                    Err(PollError::Other(e)) => {
                        if crate::engine::debug_level() >= 1 {
                            eprintln!("[toki:poll] usage error: {}", e);
                        }
                        consecutive_failures += 1;
                        let backoff =
                            (15_000i64 << consecutive_failures.min(3)).min(60_000);
                        Err((AuthStatus::Ok, Some(backoff)))
                    }
                }
            }
        };

        match result {
            Ok(usage) => {
                consecutive_failures = 0;
                backoff_until_ms = 0;
                last_auth_ok = true;
                let plan = profile.as_ref().map(|p| p.plan.as_str()).unwrap_or("");
                let observations = observations_from_usage(&usage, plan, now);
                peak_hint = observations
                    .iter()
                    .map(|o| o.used_percent)
                    .fold(0.0, f64::max);
                for obs in &observations {
                    if confirm_due {
                        confirmed.insert(crate::windows::floor_to_minute(obs.resets_at_ms));
                    }
                    if let Some(w) = tracker.observe(obs) {
                        let _ = db_tx.send(DbOp::WriteWindow(Box::new(w)));
                    }
                }
                let extra_enabled = usage
                    .extra_usage
                    .as_ref()
                    .and_then(|e| e.is_enabled)
                    .unwrap_or(false);
                hub.publish(|st| {
                    st.auth_status = AuthStatus::Ok;
                    st.last_success_ms = now;
                    st.last_poll_ms = now;
                    st.plan = plan.to_string();
                    st.extra_usage_enabled = extra_enabled;
                    st.done_seq = st.want_seq;
                });
            }
            Err((status, backoff)) => {
                if status != AuthStatus::Ok {
                    last_auth_ok = false;
                }
                if let Some(b) = backoff {
                    backoff_until_ms = now + b;
                }
                hub.publish(|st| {
                    if status != AuthStatus::Ok {
                        st.auth_status = status;
                    } else {
                        // Credentials read fine; only the fetch failed
                        // transiently. Publish Ok so a fresh daemon's default
                        // Missing state never masquerades as "logged out".
                        st.auth_status = AuthStatus::Ok;
                    }
                    st.last_poll_ms = now;
                    st.done_seq = st.want_seq;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_credentials_classifies() {
        let ok = r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":99999999999999}}"#;
        assert!(parse_credentials(ok).is_ok());

        let expired = r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":1}}"#;
        assert!(matches!(parse_credentials(expired), Err(AuthStatus::Expired)));

        let missing = r#"{"otherKey":{}}"#;
        assert!(matches!(parse_credentials(missing), Err(AuthStatus::Missing)));

        let empty_token = r#"{"claudeAiOauth":{"accessToken":""}}"#;
        assert!(matches!(parse_credentials(empty_token), Err(AuthStatus::Missing)));

        assert!(matches!(parse_credentials("not json"), Err(AuthStatus::Unreadable)));
    }

    #[test]
    fn observations_map_buckets_to_limits() {
        let usage = UsageResponseRaw {
            five_hour: Some(UsageBucketRaw {
                utilization: Some(38.0),
                resets_at: Some("2026-08-02T13:10:00.322456+00:00".into()),
            }),
            seven_day: Some(UsageBucketRaw {
                utilization: Some(22.0),
                resets_at: Some("2026-08-04T12:00:00.322477+00:00".into()),
            }),
            seven_day_sonnet: None,
            seven_day_opus: Some(UsageBucketRaw {
                utilization: None, // no utilization → skipped
                resets_at: Some("2026-08-04T12:00:00Z".into()),
            }),
            extra_usage: Some(ExtraUsageRaw { is_enabled: Some(true) }),
        };
        let obs = observations_from_usage(&usage, "default_claude_max_5x", 1_000);
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].limit_id, "five_hour");
        assert_eq!(obs[0].window_minutes, 300);
        assert!(obs[0].has_credits);
        assert_eq!(obs[0].plan_type.as_deref(), Some("default_claude_max_5x"));
        assert_eq!(obs[1].limit_id, "seven_day");
        // Microsecond-jittered ISO strings parse to stable ms.
        assert_eq!(obs[1].resets_at_ms, parse_iso_ms("2026-08-04T12:00:00.322477+00:00").unwrap());
    }

    #[test]
    fn hub_refresh_completes_or_times_out() {
        let hub = Arc::new(PollerHub::new(None, None, true));
        // No poller running: request times out and reports refreshing=true.
        let (_st, timed_out) = hub.request_refresh(Duration::from_millis(50));
        assert!(timed_out);

        // Simulate a poller completing the attempt.
        let hub2 = hub.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            hub2.publish(|st| {
                st.auth_status = AuthStatus::Ok;
                st.done_seq = st.want_seq;
            });
        });
        let (st, timed_out) = hub.request_refresh(Duration::from_millis(500));
        t.join().unwrap();
        assert!(!timed_out);
        assert_eq!(st.auth_status, AuthStatus::Ok);
    }

    #[test]
    fn codex_auth_status_classifies() {
        let dir = std::env::temp_dir().join(format!("toki-codex-auth-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let root = dir.to_string_lossy().to_string();

        assert_eq!(codex_auth_status(&root), AuthStatus::Missing);
        std::fs::write(dir.join("auth.json"), r#"{"tokens":{"access_token":"x"}}"#).unwrap();
        assert_eq!(codex_auth_status(&root), AuthStatus::Ok);
        std::fs::write(dir.join("auth.json"), r#"{"tokens":null}"#).unwrap();
        assert_eq!(codex_auth_status(&root), AuthStatus::Missing);
        std::fs::write(dir.join("auth.json"), "garbage").unwrap();
        assert_eq!(codex_auth_status(&root), AuthStatus::Unreadable);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
