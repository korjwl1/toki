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

/// Normalises a stored poll timestamp against the current wall clock.
///
/// Every poll gate is `now - last_poll_ms >= SOME_FLOOR`. A wall clock can move
/// backwards — NTP stepping a drifted laptop after sleep is the ordinary case,
/// not a pathological one — and when it does that difference goes negative,
/// every gate closes, and polling stops until the clock climbs back to where it
/// was. That can be hours, and Claude windows are the one thing here that
/// cannot be rebuilt from a file afterwards, so the gap is permanent.
///
/// A `last_poll_ms` in the future is therefore read as "never polled". The cost
/// is at most one extra poll, which the 30s floor immediately re-establishes.
fn normalise_last_poll(last_poll_ms: i64, now: i64) -> i64 {
    if last_poll_ms > now { 0 } else { last_poll_ms }
}

/// Whether a value cached at `cached_at` is still inside `ttl_ms`.
///
/// The naive `now - cached_at < ttl` is wrong in one direction that matters: a
/// backward clock step makes that difference NEGATIVE, which is less than any
/// TTL, so the entry stops expiring until the wall clock climbs back past
/// `cached_at + ttl`. A thirty-second auth cache would then hold whatever it
/// last saw for the length of the skew — the daemon would keep reporting
/// `auth ok` after the user signed out, because the cache it reads can no
/// longer go stale.
///
/// A timestamp from the future is treated as expired, which costs one refresh.
fn cache_is_fresh(cached_at: i64, ttl_ms: i64, now: i64) -> bool {
    let age = now - cached_at;
    age >= 0 && age < ttl_ms
}

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
    /// Account scope currently being collected under ("" until resolved).
    pub account: String,
    /// Claude extra-usage (pay-per-overflow) enabled on the account.
    pub extra_usage_enabled: bool,
    /// Provider-reported account shape (subscription vs seat-based, tier
    /// flags). Response-only, like `plan`: it describes the account, not any
    /// stored window, so carrying it costs no schema change.
    pub account_shape: AccountShape,
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
            account: String::new(),
            extra_usage_enabled: false,
            account_shape: AccountShape::default(),
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
            if cache_is_fresh(cached.0, TTL_MS, now) && !cached.1.is_empty() {
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
            if cache_is_fresh(cached.0, TTL_MS, now) {
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
            if cache_is_fresh(cached.0, TTL_MS, now) {
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

#[derive(Deserialize, Default)]
struct UsageResponseRaw {
    five_hour: Option<UsageBucketRaw>,
    seven_day: Option<UsageBucketRaw>,
    seven_day_sonnet: Option<UsageBucketRaw>,
    seven_day_opus: Option<UsageBucketRaw>,
    extra_usage: Option<ExtraUsageRaw>,
    /// The endpoint's current shape. The legacy top-level buckets above only
    /// carry `session` and the unscoped weekly; model-scoped weekly limits
    /// moved here (`seven_day_sonnet` now returns null even while a scoped
    /// weekly limit is live), so reading only the old keys silently loses a
    /// whole limit the user is actually consuming.
    #[serde(default)]
    limits: Vec<LimitEntryRaw>,
    /// Present on organizations with a member dashboard — a team/enterprise
    /// signal that arrives on the usage response rather than the profile.
    member_dashboard_available: Option<bool>,
}

#[derive(serde::Deserialize, Debug, Default)]
struct LimitEntryRaw {
    /// "session" | "weekly_all" | "weekly_scoped" | (future kinds)
    kind: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<LimitScopeRaw>,
}

#[derive(serde::Deserialize, Debug, Default)]
struct LimitScopeRaw {
    model: Option<LimitScopeModelRaw>,
}

#[derive(serde::Deserialize, Debug, Default)]
struct LimitScopeModelRaw {
    /// A stable machine identifier when the endpoint sends one. It is null
    /// today, which is why `display_name` is still read — but a display name
    /// is a UI string that localizes and gets rebranded, and this value ends
    /// up hashed into a storage key.
    id: Option<String>,
    display_name: Option<String>,
}

/// Builds the `limit_id` for a model-scoped weekly limit.
///
/// This value is hashed into the window storage key, so it has to stay put for
/// as long as the limit does: if it drifts, one window's history silently
/// splits in two. `id` wins over `display_name` for that reason. When the
/// endpoint starts sending `id` the identifier does change once, and that is
/// deliberate — a new identifier opens a new window and the old rows remain as
/// history rather than being rewritten.
///
/// The output is normalised so the key never carries whatever punctuation or
/// casing the endpoint happens to use ("Fable 5" and "fable-5" must not be two
/// windows), and is truncated on a character boundary — `String::truncate`
/// panics on a byte index that splits a multi-byte character, which a
/// localised display name can produce.
fn scoped_limit_id(model: &LimitScopeModelRaw) -> Option<String> {
    let raw = [model.id.as_deref(), model.display_name.as_deref()]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())?;

    let mut id = String::from("weekly_");
    let mut pending_sep = false;
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            if pending_sep {
                id.push('_');
                pending_sep = false;
            }
            id.extend(ch.to_lowercase());
        } else if id.len() > "weekly_".len() {
            // Collapse any run of separators, and never emit a trailing one.
            pending_sep = true;
        }
    }

    if id.len() > crate::windows::MAX_LIMIT_ID_LEN {
        let cut = id
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= crate::windows::MAX_LIMIT_ID_LEN)
            .last()
            .unwrap_or(0);
        id.truncate(cut);
        while id.ends_with('_') {
            id.pop();
        }
    }

    (id.len() > "weekly_".len()).then_some(id)
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
    /// Account-shape fields the endpoint already sends. They are passed through
    /// verbatim rather than classified here: the provider owns this vocabulary
    /// and adds to it, so a daemon-side enum would silently drop values it was
    /// not compiled against. A consumer that cannot recognise a value must be
    /// able to see that it did not recognise it.
    account: AccountShape,
}

/// Provider-reported account shape. Every field is optional because the
/// endpoint is free to omit any of them, and an absent field is a distinct
/// state from a known-false one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountShape {
    /// e.g. "claude_max". The provider's own account classification.
    pub organization_type: Option<String>,
    /// e.g. "stripe_subscription". Positive evidence of a subscription; its
    /// absence is NOT evidence of an API account.
    pub billing_type: Option<String>,
    /// Non-null on seat-based (team/enterprise) organizations.
    pub seat_tier: Option<String>,
    /// e.g. "active".
    pub subscription_status: Option<String>,
    pub has_claude_max: Option<bool>,
    pub has_claude_pro: Option<bool>,
    /// Set from the usage endpoint, not the profile: a team/org signal.
    pub member_dashboard_available: Option<bool>,
}

fn fetch_profile(token: &str) -> Option<ProfileInfo> {
    let resp = ureq::get(PROFILE_URL)
        .set("Authorization", &format!("Bearer {}", token))
        .set("anthropic-beta", OAUTH_BETA)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?;
    parse_profile(&resp.into_string().ok()?)
}

/// Split from the fetch so the shape extraction is testable without a network
/// call — the fields below are exactly the ones a silent parse change would
/// drop, and there was no way to assert on them while this lived inside the
/// request.
fn parse_profile(body: &str) -> Option<ProfileInfo> {
    #[derive(Deserialize)]
    struct ProfileRaw {
        account: Option<AccountRaw>,
        organization: Option<OrgRaw>,
    }
    #[derive(Deserialize)]
    struct AccountRaw {
        uuid: Option<String>,
        has_claude_max: Option<bool>,
        has_claude_pro: Option<bool>,
    }
    #[derive(Deserialize)]
    struct OrgRaw {
        rate_limit_tier: Option<String>,
        organization_type: Option<String>,
        billing_type: Option<String>,
        seat_tier: Option<String>,
        subscription_status: Option<String>,
    }
    let p: ProfileRaw = serde_json::from_str(body).ok()?;
    let acct = p.account.unwrap_or(AccountRaw {
        uuid: None,
        has_claude_max: None,
        has_claude_pro: None,
    });
    let account_scope = acct
        .uuid
        .filter(|u| !u.is_empty())
        .map(|u| format!("{:016x}", crate::windows::hash_str(&u)))
        .unwrap_or_else(|| "default".to_string());
    let org = p.organization;
    let plan = org
        .as_ref()
        .and_then(|o| o.rate_limit_tier.clone())
        .unwrap_or_default();
    // Blank strings are the endpoint saying nothing, not saying "".
    let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
    let account = AccountShape {
        organization_type: nonempty(org.as_ref().and_then(|o| o.organization_type.clone())),
        billing_type: nonempty(org.as_ref().and_then(|o| o.billing_type.clone())),
        seat_tier: nonempty(org.as_ref().and_then(|o| o.seat_tier.clone())),
        subscription_status: nonempty(org.as_ref().and_then(|o| o.subscription_status.clone())),
        has_claude_max: acct.has_claude_max,
        has_claude_pro: acct.has_claude_pro,
        member_dashboard_available: None,
    };
    Some(ProfileInfo { account_scope, plan, account })
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
    drop(push);

    // Model-scoped weekly limits live ONLY in `limits[]`; the legacy
    // `seven_day_sonnet`/`seven_day_opus` keys return null even while such a
    // limit is being consumed. Kinds already covered by the legacy keys are
    // skipped so one limit never becomes two rows.
    for l in &usage.limits {
        let kind = l.kind.as_deref().unwrap_or("");
        if kind != "weekly_scoped" {
            // session / weekly_all duplicate five_hour / seven_day.
            continue;
        }
        let Some(limit_id) = l
            .scope
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .and_then(scoped_limit_id)
        else {
            continue;
        };
        let (Some(pct), Some(reset_str)) = (l.percent, l.resets_at.as_deref()) else {
            continue;
        };
        let Some(resets_at_ms) = parse_iso_ms(reset_str) else {
            continue;
        };
        out.push(WindowObservation {
            limit_id,
            window_minutes: 10_080,
            used_percent: pct,
            resets_at_ms,
            plan_type: if plan.is_empty() { None } else { Some(plan.to_string()) },
            limit_reached: pct >= 99.995,
            has_credits,
            anchor_stable: true,
            ts_ms,
        });
    }
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
    // The token the server last rejected (401/403). Re-probing with the SAME
    // token can only produce another 401, so the HTTP call is skipped until a
    // different token appears — which is exactly what a re-login produces, so
    // recovery stays immediate.
    let mut rejected_token_hash: u64 = 0;
    // Reset instants (minute-floored) whose pre-reset confirm sample was taken.
    let mut confirmed: std::collections::HashSet<i64> = std::collections::HashSet::new();

    loop {
        let now = now_ms();

        // One allocation, two deadlines: both the confirm sample and the
        // finalize wake are derived from the same open-window reset instants.
        let open_resets = tracker.open_reset_times();

        // Next hard deadline: the earliest pending pre-reset confirm sample.
        let next_confirm = open_resets
            .iter()
            .copied()
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
        // Wake shortly after a window's reset to finalize it. Without this the
        // row stays finalized=false until the next poll — up to an hour on an
        // idle machine — so statistics exclude it and the monitor can still
        // read it as live. Costs one wake per reset and no provider call.
        if let Some(finalize_at) = open_resets
            .iter()
            .map(|r| r + crate::windows::FINALIZE_GRACE_MS + 1_000)
            .filter(|t| *t > now)
            .min()
        {
            deadline = deadline.min(finalize_at);
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

        // See `normalise_last_poll`: a backward clock step must not wedge the
        // poller until the clock catches up.
        last_poll_ms = normalise_last_poll(last_poll_ms, now);

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
                    // Claude Code rotates its access token on its own schedule,
                    // so a changed token usually means the SAME account. Only
                    // re-resolve the profile; do not quarantine to "unknown"
                    // here — a failing profile endpoint would then split every
                    // observation into a second row for 10 minutes, and the
                    // published account would disagree with what is written.
                    // If the resolved scope really differs, the tracker's open
                    // windows are re-keyed below.
                    profile = None;
                    profile_attempt_ms = 0; // token change bypasses the retry backoff
                }
                last_token_hash = token_hash;
                if rejected_token_hash != 0 && token_hash == rejected_token_hash {
                    // Known-bad token: no API call. Keep the published state
                    // (already Expired) and wait for a new one.
                    hub.publish(|st| {
                        st.last_poll_ms = now;
                        st.done_seq = st.want_seq;
                    });
                    continue;
                }
                match fetch_usage(&creds.access_token) {
                    Ok(usage) => {
                        // Account/plan resolution piggybacks on a proven token.
                        if (profile.is_none() || now - profile_fetched_ms > PROFILE_REFRESH_MS)
                            && now - profile_attempt_ms > PROFILE_RETRY_MS
                        {
                            profile_attempt_ms = now;
                            if let Some(p) = fetch_profile(&creds.access_token) {
                                if tracker.account() == "unknown" {
                                    // Opened before the account was known:
                                    // adopt the resolved scope (same windows).
                                    tracker.reattribute_open(&p.account_scope);
                                } else if tracker.account() != p.account_scope {
                                    // A genuinely DIFFERENT account: the old
                                    // account's windows will never be observed
                                    // again, so close them under their own
                                    // keys. Re-keying them here would have
                                    // blended A's peak into B's row and let
                                    // B's activity credit A's window.
                                    for w in tracker.close_all() {
                                        let _ = db_tx.send(DbOp::WriteWindow(Box::new(w)));
                                    }
                                }
                                tracker.set_account(&p.account_scope);
                                let scope = p.account_scope.clone();
                                profile = Some(p);
                                profile_fetched_ms = now;
                                hub.publish(|st| st.account = scope);
                            }
                        }
                        Ok(usage)
                    }
                    Err(PollError::AuthRejected) => {
                        rejected_token_hash = token_hash;
                        Err((AuthStatus::Expired, None))
                    }
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
                rejected_token_hash = 0;
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
                // The profile is refreshed at most daily, so the shape it
                // carries has to survive the polls in between. Only the
                // member-dashboard flag comes from this response.
                let mut shape = profile
                    .as_ref()
                    .map(|p| p.account.clone())
                    .unwrap_or_default();
                if usage.member_dashboard_available.is_some() {
                    shape.member_dashboard_available = usage.member_dashboard_available;
                }
                hub.publish(|st| {
                    st.auth_status = AuthStatus::Ok;
                    st.last_success_ms = now;
                    st.last_poll_ms = now;
                    st.plan = plan.to_string();
                    st.extra_usage_enabled = extra_enabled;
                    st.account_shape = shape;
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
                    if status == AuthStatus::Missing {
                        // Logged out: stop naming the previous account, or the
                        // monitor keeps filtering rows against a dead scope.
                        st.account.clear();
                    }
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


    /// The endpoint moved model-scoped weekly limits out of the top-level keys
    /// and into `limits[]`; `seven_day_sonnet` now returns null while a scoped
    /// weekly limit is actively being consumed. Reading only the legacy keys
    /// silently loses a limit the user is spending against.
    #[test]
    fn scoped_weekly_limit_is_captured_from_the_limits_array() {
        let raw: UsageResponseRaw = serde_json::from_str(
            r#"{
              "five_hour": {"utilization": 16.0, "resets_at": "2026-08-08T09:10:00+00:00"},
              "seven_day": {"utilization": 28.0, "resets_at": "2026-08-11T12:00:00+00:00"},
              "seven_day_sonnet": null,
              "limits": [
                {"kind":"session","group":"session","percent":16,"resets_at":"2026-08-08T09:10:00+00:00"},
                {"kind":"weekly_all","group":"weekly","percent":28,"resets_at":"2026-08-11T12:00:00+00:00"},
                {"kind":"weekly_scoped","group":"weekly","percent":13,
                 "resets_at":"2026-08-11T12:00:00+00:00",
                 "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null}}
              ]
            }"#,
        )
        .unwrap();
        let obs = observations_from_usage(&raw, "max_5x", 1_800_000_000_000);
        let ids: Vec<&str> = obs.iter().map(|o| o.limit_id.as_str()).collect();
        assert!(ids.contains(&"five_hour"), "{ids:?}");
        assert!(ids.contains(&"seven_day"), "{ids:?}");
        assert!(ids.contains(&"weekly_fable"), "scoped limit missing: {ids:?}");
        // session / weekly_all must not double up with the legacy keys.
        assert_eq!(ids.len(), 3, "one limit became two rows: {ids:?}");
        let fable = obs.iter().find(|o| o.limit_id == "weekly_fable").unwrap();
        assert_eq!(fable.window_minutes, 10_080);
        assert!((fable.used_percent - 13.0).abs() < 0.01);
    }

    /// `display_name` is a UI string. It is what the endpoint sends today
    /// because `id` is null, but the moment a stable slug appears it must win:
    /// this value is hashed into the window storage key, and a key that tracks
    /// a display name gets re-cut every time marketing renames a model.
    #[test]
    fn a_stable_model_id_is_preferred_over_the_display_name() {
        let m = LimitScopeModelRaw {
            id: Some("claude-fable-5".into()),
            display_name: Some("Fable".into()),
        };
        assert_eq!(scoped_limit_id(&m).as_deref(), Some("weekly_claude_fable_5"));
    }

    #[test]
    fn the_display_name_is_used_only_when_no_id_is_sent() {
        let m = LimitScopeModelRaw { id: None, display_name: Some("Fable".into()) };
        assert_eq!(scoped_limit_id(&m).as_deref(), Some("weekly_fable"));
        // An empty or blank id must not beat a usable display name.
        let blank = LimitScopeModelRaw {
            id: Some("   ".into()),
            display_name: Some("Fable".into()),
        };
        assert_eq!(scoped_limit_id(&blank).as_deref(), Some("weekly_fable"));
    }

    /// Punctuation and casing are the endpoint's presentation choice. If they
    /// reached the key, "Fable 5" and "fable-5" would be two windows for one
    /// limit.
    #[test]
    fn punctuation_and_casing_do_not_fork_a_window() {
        let of = |s: &str| {
            scoped_limit_id(&LimitScopeModelRaw { id: Some(s.into()), display_name: None })
                .unwrap()
        };
        assert_eq!(of("Fable 5"), "weekly_fable_5");
        assert_eq!(of("fable-5"), "weekly_fable_5");
        assert_eq!(of("  Fable   5  "), "weekly_fable_5");
        assert_eq!(of("Fable/5"), "weekly_fable_5");
        // No trailing or doubled separator ever reaches the key.
        assert!(!of("Fable 5 ").ends_with('_'));
        assert!(!of("Fable  5").contains("__"));
    }

    /// The previous code did `String::truncate(MAX_LIMIT_ID_LEN)` on a value
    /// derived from a display name. `truncate` takes a BYTE index and panics
    /// when it splits a multi-byte character, so one localised model name would
    /// have taken down the poller thread.
    #[test]
    fn an_over_long_multibyte_name_truncates_instead_of_panicking() {
        let long = "\u{d55c}".repeat(100); // 3 bytes each, far past the limit
        let id = scoped_limit_id(&LimitScopeModelRaw {
            id: Some(long),
            display_name: None,
        })
        .expect("a long name still yields an id");
        assert!(id.len() <= crate::windows::MAX_LIMIT_ID_LEN, "len {}", id.len());
        assert!(id.is_char_boundary(id.len()), "truncated mid-character");
        // Long ASCII truncates too, and never to a trailing separator.
        let ascii = scoped_limit_id(&LimitScopeModelRaw {
            id: Some("a-".repeat(60)),
            display_name: None,
        })
        .unwrap();
        assert!(ascii.len() <= crate::windows::MAX_LIMIT_ID_LEN);
        assert!(!ascii.ends_with('_'));
    }

    /// A name with nothing alphanumeric in it yields no identifier at all
    /// rather than a bare `weekly_` that every such limit would collide on.
    #[test]
    fn a_nameless_scope_opens_no_window() {
        for raw in ["", "   ", "---", "///"] {
            let m = LimitScopeModelRaw { id: Some(raw.into()), display_name: None };
            assert!(scoped_limit_id(&m).is_none(), "{raw:?} must not yield an id");
        }
        assert!(scoped_limit_id(&LimitScopeModelRaw::default()).is_none());
    }

    /// The account fields were being parsed away: the endpoint sends the
    /// account's shape on every profile call and the daemon kept only the
    /// rate-limit tier. Without these, a consumer can only infer "this is a
    /// subscription" from the ABSENCE of something, which is exactly the
    /// inference that misfires on a logged-out or polling-disabled account.
    ///
    /// The fixture is the live 2026-08-26 response with identifiers removed.
    #[test]
    fn the_profile_carries_the_accounts_shape() {
        #[derive(Deserialize)]
        struct P {
            account: Option<serde_json::Value>,
            organization: Option<serde_json::Value>,
        }
        let raw = r#"{
          "account": {"uuid":"u","full_name":"n","email":"e",
                      "has_claude_max": true, "has_claude_pro": false,
                      "created_at":"2025-04-28T04:17:40Z"},
          "organization": {"uuid":"o","name":"n",
                           "organization_type":"claude_max",
                           "billing_type":"stripe_subscription",
                           "rate_limit_tier":"default_claude_max_5x",
                           "seat_tier":null,
                           "subscription_status":"active"},
          "application": {"slug":"claude-code"}
        }"#;
        // Deserializing must not fail on the fields we do not model.
        let p: P = serde_json::from_str(raw).expect("profile shape parses");
        assert!(p.account.is_some() && p.organization.is_some());

        let shape = parse_profile(raw).expect("profile parses").account;
        assert_eq!(shape.organization_type.as_deref(), Some("claude_max"));
        assert_eq!(shape.billing_type.as_deref(), Some("stripe_subscription"));
        assert_eq!(shape.subscription_status.as_deref(), Some("active"));
        assert_eq!(shape.has_claude_max, Some(true));
        assert_eq!(shape.has_claude_pro, Some(false));
        // A seat tier of null is "not a seat-based org", and must stay None
        // rather than becoming an empty string that reads as a real value.
        assert_eq!(shape.seat_tier, None);
    }

    /// An absent field and a false field are different states. Defaulting the
    /// absent ones to false would let a consumer conclude "not a subscription"
    /// from a response that simply did not mention it.
    #[test]
    fn absent_account_fields_stay_absent() {
        let shape = parse_profile(r#"{"organization":{"rate_limit_tier":"x"}}"#)
            .expect("profile parses")
            .account;
        assert_eq!(shape.organization_type, None);
        assert_eq!(shape.billing_type, None);
        assert_eq!(shape.has_claude_max, None, "absent must not become false");
        assert_eq!(shape.has_claude_pro, None);
    }

    /// A blank string is the endpoint declining to answer, not an answer.
    #[test]
    fn blank_account_strings_are_treated_as_absent() {
        let shape = parse_profile(
            r#"{"organization":{"organization_type":"","billing_type":"   ","seat_tier":"team"}}"#,
        )
        .expect("profile parses")
        .account;
        assert_eq!(shape.organization_type, None);
        assert_eq!(shape.billing_type, None);
        assert_eq!(shape.seat_tier.as_deref(), Some("team"));
    }

    /// The usage response carries one account-shape signal of its own.
    #[test]
    fn the_usage_response_carries_the_member_dashboard_flag() {
        let raw: UsageResponseRaw = serde_json::from_str(
            r#"{"five_hour":null,"member_dashboard_available":false}"#,
        )
        .unwrap();
        assert_eq!(raw.member_dashboard_available, Some(false));
        let absent: UsageResponseRaw = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.member_dashboard_available, None);
    }

    /// The endpoint's top-level bucket list is open-ended and grows with
    /// codenames (the live response carried `nimbus_quill`, `tangelo`,
    /// `iguana_necktie` and others alongside the four we name). Unknown keys
    /// must not break the parse, and a bucket with no reset time must not
    /// become a window.
    #[test]
    fn unknown_codename_buckets_do_not_break_the_parse() {
        let raw: UsageResponseRaw = serde_json::from_str(
            r#"{
              "five_hour": {"utilization": 20.0, "resets_at": "2026-08-25T22:20:00+00:00"},
              "nimbus_quill": {"utilization": 0.0, "resets_at": null},
              "tangelo": null, "iguana_necktie": null, "amber_ladder": null,
              "limits": []
            }"#,
        )
        .expect("unknown buckets must not fail the parse");
        let obs = observations_from_usage(&raw, "max_5x", 1_800_000_000_000);
        let ids: Vec<&str> = obs.iter().map(|o| o.limit_id.as_str()).collect();
        assert_eq!(ids, vec!["five_hour"], "a bucket with no reset is not a window: {ids:?}");
    }

    /// The poller wedges without this.
    ///
    /// Every gate is `now - last_poll_ms >= SOME_FLOOR`. Step the wall clock
    /// backwards — which NTP does to a drifted laptop coming out of sleep —
    /// and that difference is negative, so every gate closes and polling stops
    /// until the clock climbs back to where it was. Claude windows are polled
    /// only; nothing on disk can rebuild the ones missed in that interval.
    #[test]
    fn a_backward_clock_step_does_not_wedge_the_poller() {
        let now = 1_800_000_000_000_i64;

        // The ordinary case is untouched.
        assert_eq!(normalise_last_poll(now - 60_000, now), now - 60_000);
        assert_eq!(normalise_last_poll(0, now), 0);
        assert_eq!(normalise_last_poll(now, now), now, "equal is not in the future");

        // An hour-long backward step. Without the guard `now - last_poll` is
        // -3_600_000 and every floor comparison fails.
        let stale = normalise_last_poll(now + 3_600_000, now);
        assert_eq!(stale, 0);
        assert!(now - stale >= POLL_FLOOR_MS, "the 30s floor must be satisfiable again");

        // And the floor still applies from there — the guard buys one poll, not
        // an open door.
        let after_poll = normalise_last_poll(now, now);
        assert!(now - after_poll < POLL_FLOOR_MS, "an immediate second poll is still refused");
    }

    /// A cache that cannot go stale is worse than no cache.
    ///
    /// `now - cached_at < ttl` reads TRUE for a negative age, so a backward
    /// clock step freezes every TTL here until the wall clock climbs back past
    /// the entry's own timestamp. These caches hold auth status, so a frozen
    /// one keeps reporting `auth ok` after the user has signed out — for the
    /// length of the skew, not for the thirty seconds the TTL promises.
    #[test]
    fn a_backward_clock_step_does_not_freeze_a_cache() {
        let now = 1_800_000_000_000_i64;
        let ttl = 30_000_i64;

        assert!(cache_is_fresh(now - 1_000, ttl, now), "a one-second-old entry is fresh");
        assert!(!cache_is_fresh(now - ttl, ttl, now), "exactly at the TTL is expired");
        assert!(!cache_is_fresh(now - ttl - 1, ttl, now), "past the TTL is expired");
        assert!(cache_is_fresh(now, ttl, now), "written this instant");

        // The bug: an hour-long backward step leaves every entry stamped in the
        // future, and `now - cached_at` is -3_600_000, which is < any ttl.
        assert!(
            !cache_is_fresh(now + 3_600_000, ttl, now),
            "an entry from the future must expire, not become permanently fresh"
        );
    }

    /// An older server that only sends the legacy keys must keep working.
    #[test]
    fn missing_limits_array_falls_back_to_legacy_keys() {
        let raw: UsageResponseRaw = serde_json::from_str(
            r#"{"five_hour": {"utilization": 5.0, "resets_at": "2026-08-08T09:10:00+00:00"}}"#,
        )
        .unwrap();
        let obs = observations_from_usage(&raw, "", 1_800_000_000_000);
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].limit_id, "five_hour");
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
            limits: Vec::new(),
            member_dashboard_available: None,
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
