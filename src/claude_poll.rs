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
    /// Generic "re-evaluate now" nudge (settings change). A bare notify is
    /// indistinguishable from a spurious wakeup: wait_timeout_while re-checks
    /// its predicate and sleeps out the REMAINING duration, so a state flag
    /// is required to actually shorten an hour-long idle sleep.
    wake: bool,
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
    /// Set by the poller thread on startup — false means no poller exists
    /// (Codex-only config, or spawn failure), so freshness waits must fail
    /// fast instead of burning the 2s timeout on every forced refresh.
    poller_running: std::sync::atomic::AtomicBool,
    /// Cached fallback auth classification for the polling-disabled path:
    /// the WINDOWS handler would otherwise spawn a `security` subprocess per
    /// widget poll. (ts_ms, status)
    fallback_auth: Mutex<(i64, AuthStatus)>,
    /// Same 30s cache for Codex: auth.json was re-read and DOM-parsed on
    /// every WINDOWS request.
    codex_auth_cache: Mutex<(i64, AuthStatus)>,
    /// 30s cache for the Codex account scope (same file, same rate).
    codex_account_cache: Mutex<(i64, String)>,
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
            poller_running: std::sync::atomic::AtomicBool::new(false),
            fallback_auth: Mutex::new((0, AuthStatus::Missing)),
            codex_auth_cache: Mutex::new((0, AuthStatus::Missing)),
            codex_account_cache: Mutex::new((0, String::new())),
        }
    }

    /// Codex account scope with a 30s cache (WINDOWS-handler rate).
    pub fn codex_account_cached(&self) -> String {
        const TTL_MS: i64 = 30_000;
        let now = now_ms();
        {
            let cached = self.codex_account_cache.lock().unwrap_or_else(|e| e.into_inner());
            if now - cached.0 < TTL_MS && !cached.1.is_empty() {
                return cached.1.clone();
            }
        }
        let scope = self
            .codex_root
            .as_deref()
            .map(crate::providers::codex::account_scope)
            .unwrap_or_else(|| "unknown".to_string());
        *self.codex_account_cache.lock().unwrap_or_else(|e| e.into_inner()) = (now, scope.clone());
        scope
    }

    /// Codex auth classification with a 30s cache (WINDOWS-handler rate).
    pub fn codex_auth_cached(&self) -> AuthStatus {
        const TTL_MS: i64 = 30_000;
        let now = now_ms();
        {
            let cached = self.codex_auth_cache.lock().unwrap_or_else(|e| e.into_inner());
            if now - cached.0 < TTL_MS {
                return cached.1;
            }
        }
        let status = self
            .codex_root
            .as_deref()
            .map(codex_auth_status)
            .unwrap_or(AuthStatus::Missing);
        *self.codex_auth_cache.lock().unwrap_or_else(|e| e.into_inner()) = (now, status);
        status
    }

    pub fn poller_running(&self) -> bool {
        self.poller_running.load(Ordering::Relaxed)
    }

    fn set_poller_running(&self, running: bool) {
        self.poller_running.store(running, Ordering::Relaxed);
    }

    /// Claude auth classification with a 30s cache — used only when active
    /// polling is disabled (the poller's published state is authoritative
    /// otherwise).
    pub fn claude_auth_cached(&self) -> AuthStatus {
        const TTL_MS: i64 = 30_000;
        let now = now_ms();
        {
            let cached = self.fallback_auth.lock().unwrap_or_else(|e| e.into_inner());
            if now - cached.0 < TTL_MS {
                return cached.1;
            }
        }
        let status = match &self.claude_root {
            Some(root) => match read_claude_credentials(root) {
                Ok(_) => AuthStatus::Ok,
                Err(s) => s,
            },
            None => AuthStatus::Missing,
        };
        *self.fallback_auth.lock().unwrap_or_else(|e| e.into_inner()) = (now, status);
        status
    }

    pub fn polling_enabled(&self) -> bool {
        self.polling_enabled.load(Ordering::Relaxed)
    }

    /// Hot-reload hook: flips active polling without a daemon restart. The
    /// poller thread checks this before each provider call; the passive
    /// (window_tracking) pipeline is engine-embedded and stays restart-bound.
    pub fn set_polling_enabled(&self, enabled: bool) {
        self.polling_enabled.store(enabled, Ordering::Relaxed);
        let (lock, cvar) = &self.signal;
        let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
        s.wake = true;
        cvar.notify_all();
    }

    /// Called by the writer after each flush: tokens are flowing.
    /// Poison recovery everywhere in this struct (into_inner, never a silent
    /// drop): a dropped stop() would wedge Handle::shutdown on the poller join.
    pub fn notify_token_flow(&self) {
        self.last_token_flow_ms.store(now_ms(), Ordering::Relaxed);
        let (lock, cvar) = &self.signal;
        let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
        s.token_flow = true;
        cvar.notify_all();
    }

    pub fn stop(&self) {
        let (lock, cvar) = &self.signal;
        let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
        s.stop = true;
        cvar.notify_all();
    }

    pub fn state_snapshot(&self) -> PublishedState {
        self.state.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Request a fresh poll and wait (bounded) for the poller to complete one
    /// attempt. Returns the resulting state; on timeout the caller serves stale
    /// data marked `refreshing: true`. Fails fast when no poller thread exists.
    pub fn request_refresh(&self, wait: Duration) -> (PublishedState, bool) {
        if !self.poller_running() {
            return (self.state_snapshot(), false);
        }
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
            // into_inner like every other hub method: dropping this signal
            // would burn the caller's full wait and report refreshing forever.
            let mut s = lock.lock().unwrap_or_else(|e| e.into_inner());
            s.refresh = true;
            cvar.notify_all();
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
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut st);
        cvar.notify_all();
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
        // Bounded wait: a locked keychain / pending ACL prompt can block
        // `security` indefinitely, which would wedge the poller AND
        // Handle::shutdown's join. 10s then kill → transient Unreadable.
        let mut child = std::process::Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| AuthStatus::Unreadable)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AuthStatus::Unreadable);
                }
            }
        };
        let mut stdout = Vec::new();
        if let Some(mut out_pipe) = child.stdout.take() {
            use std::io::Read;
            let _ = out_pipe.read_to_end(&mut stdout);
        }
        let out = std::process::Output { status, stdout, stderr: Vec::new() };
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

/// Codex auth classification (thin delegate — see providers::codex).
pub fn codex_auth_status(codex_root: &str) -> AuthStatus {
    crate::providers::codex::auth_status(codex_root)
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
    db_tx: crossbeam_channel::Sender<DbOp>,
    claude_root: String,
) {
    // RAII: a panic anywhere below must not leave poller_running=true, or
    // every forced refresh burns its full 2s wait against a dead thread.
    struct RunningGuard(Arc<PollerHub>);
    impl Drop for RunningGuard {
        fn drop(&mut self) {
            self.0.set_poller_running(false);
        }
    }
    hub.set_poller_running(true);
    let _running_guard = RunningGuard(hub.clone());
    let mut tracker = WindowTracker::new();
    let mut last_poll_ms: i64 = 0;
    let mut consecutive_failures: u32 = 0;
    let mut backoff_until_ms: i64 = 0;
    let mut last_auth_ok = false;
    let mut peak_hint: f64 = 0.0;
    let mut profile: Option<ProfileInfo> = None;
    let mut profile_fetched_ms: i64 = 0;
    // Negative cache: a failing profile endpoint must not add a second API
    // request to every poll (10-min retry backoff; token changes bypass it).
    let mut profile_attempt_ms: i64 = 0;
    const PROFILE_RETRY_MS: i64 = 600_000;
    // Detects an atomic valid-token→valid-token swap (logout+login between
    // polls never trips an auth failure, but the account may have changed).
    let mut last_token_hash: u64 = 0;
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

        // Compute how long to sleep. Idle, or polling disabled at runtime
        // (set_polling_enabled notifies the condvar to re-evaluate): hourly
        // housekeeping only. The scheduled deadline respects an active
        // backoff, and a past-due confirm retries on the 30s floor — never
        // the 50ms clamp (each of those was a measured ~20Hz busy loop).
        let polling_on = hub.polling_enabled();
        let mut deadline = if !polling_on {
            now + 3_600_000
        } else if active {
            (last_poll_ms + interval).max(backoff_until_ms)
        } else {
            now + 3_600_000
        };
        if polling_on {
            if let Some(c) = next_confirm {
                let confirm_deadline = c
                    .max(last_poll_ms + POLL_FLOOR_MS)
                    .max(backoff_until_ms);
                deadline = deadline.min(confirm_deadline.max(now));
            }
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
                .wait_timeout_while(guard, wait, |s| {
                    !s.token_flow && !s.refresh && !s.wake && !s.stop
                })
                .unwrap_or_else(|e| {
                    let g = e.into_inner();
                    (g.0, g.1)
                });
            let out = (s.token_flow, s.refresh, s.stop);
            s.token_flow = false;
            s.refresh = false;
            s.wake = false;
            out
        };

        if stop {
            hub.set_poller_running(false);
            for w in tracker.flush_all() {
                let _ = db_tx.send(DbOp::WriteWindow(Box::new(w)));
            }
            break;
        }

        let now = now_ms();

        // Token-flow wakes arrive per writer flush (~1/s during activity):
        // feed them into the 30-min-gap active-time accounting. Without this
        // every Claude window carried active_ms = 0 and the duty-cycle /
        // active-mean statistics — a core product requirement — read zero.
        if token_flow {
            tracker.observe_activity(now);
        }

        // Finalize expired windows regardless of polling decisions.
        for w in tracker.finalize_expired(now) {
            confirmed.remove(&crate::windows::floor_to_minute(w.snapshot.raw_resets_at_ms));
            let _ = db_tx.send(DbOp::WriteWindow(Box::new(w)));
        }

        // ---- Decide whether to poll now ----
        // Confirm retries share the 30s floor and honor backoff: a failing
        // confirm poll (offline, 429) must not retry at the wake clamp.
        let confirm_due = next_confirm.map(|c| now >= c).unwrap_or(false)
            && now - last_poll_ms >= POLL_FLOOR_MS
            && now >= backoff_until_ms;
        let active = now - hub.last_token_flow() < ACTIVE_HORIZON_MS;
        let since_last = now - last_poll_ms;

        // Re-login path: a token-flow wake while auth is failing implies a
        // possible re-login. Rate-limited to the monitor's legacy 20s auth
        // cadence and honoring backoff — the writer wakes this loop on every
        // flush (~1/s while streaming), and an unbounded probe would hammer
        // the endpoint for the whole failure episode.
        const RELOGIN_PROBE_MS: i64 = 20_000;
        let relogin_probe = token_flow
            && !last_auth_ok
            && since_last >= RELOGIN_PROBE_MS
            && now >= backoff_until_ms;

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
                // Auth just recovered OR the token changed under us (atomic
                // re-login to a different account trips no failure) — drop the
                // cached profile so it re-resolves below (after the usage
                // fetch proves the token, so failure episodes never pay a
                // second request per attempt).
                let token_hash = crate::windows::hash_str(&creds.access_token);
                if !last_auth_ok || token_hash != last_token_hash {
                    profile = None;
                    profile_attempt_ms = 0; // token change bypasses the retry backoff
                    // Until the NEW token's profile resolves, attribution must
                    // not continue into the previous account's rows: quarantine
                    // under "unknown" (consistent with backfill's policy —
                    // unknown segments are excluded from advice).
                    if token_hash != last_token_hash && last_token_hash != 0 {
                        tracker.set_account("unknown");
                    }
                }
                last_token_hash = token_hash;
                match fetch_usage(&creds.access_token) {
                    Ok(usage) => {
                        // Account/plan resolution piggybacks on a proven token.
                        if (profile.is_none() || now - profile_fetched_ms > PROFILE_REFRESH_MS)
                            && now - profile_attempt_ms > PROFILE_RETRY_MS
                        {
                            profile_attempt_ms = now;
                            if let Some(p) = fetch_profile(&creds.access_token) {
                                tracker.set_account(&p.account_scope);
                                profile = Some(p);
                                profile_fetched_ms = now;
                            }
                        }
                        Ok(usage)
                    }
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
                // Densify on the SESSION window only: a weekly bucket sits
                // above 90% for days on a heavy plan, and 30s polling buys
                // 0.03% of extra time_to_100 precision on a 7-day window
                // while quadrupling calls to a rate-limit-sensitive endpoint.
                peak_hint = observations
                    .iter()
                    .filter(|o| o.window_minutes <= 24 * 60)
                    .map(|o| o.used_percent)
                    .fold(0.0, f64::max);
                for obs in &observations {
                    // Mark ONLY the window whose confirm span this sample
                    // actually covers — a five_hour confirm must not mark the
                    // weekly buckets (days out) as already confirmed.
                    if confirm_due && obs.resets_at_ms - now <= CONFIRM_BEFORE_RESET_MS {
                        confirmed.insert(crate::windows::floor_to_minute(obs.resets_at_ms));
                    }
                    if let Some(w) = tracker.observe(obs) {
                        let _ = db_tx.send(DbOp::WriteWindow(Box::new(w)));
                    }
                }
                // Entries whose reset passed are dead (jittered inserts can
                // miss the finalize-time removal) — prune, don't leak.
                confirmed.retain(|&t| t > now - 86_400_000);
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
        // No poller thread exists (Codex-only config): fail fast with the
        // cached state instead of burning the wait on every forced refresh.
        let t0 = std::time::Instant::now();
        let (_st, timed_out) = hub.request_refresh(Duration::from_millis(500));
        assert!(!timed_out);
        assert!(t0.elapsed() < Duration::from_millis(100));

        // With a poller: an unanswered request times out (stale + refreshing).
        hub.set_poller_running(true);
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
