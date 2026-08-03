//! Rate-limit window tracking: turn per-line `rate_limits` observations into
//! one durable row per window instance.
//!
//! Identity rule (empirically validated 2026-08-02, 4,713 observations):
//! a window instance is keyed by the resets_at of its FIRST >0% observation,
//! floored to the minute (the "anchor"). Later observations jitter by up to
//! ~93s around the true reset time, so they are merged into an open window by
//! ±120s proximity against the latest raw resets_at — but the anchor (and thus
//! the storage key) is never rewritten once created. Passively-extracted
//! unused (0%) limits slide their resets_at continuously (resets_at ≈ now +
//! window), so 0% observations never open a window UNLESS the source vouches
//! for anchor stability (`WindowObservation::anchor_stable` — Claude's active
//! poller, where a served resets_at is a real window and genuine zero-use
//! weekly periods must exist for the overall mean).
//!
//! Utilization is monotone non-decreasing within a window (observed decreases
//! are 1%p flickers or a server-side limit reset), so `peak = max(samples)` is
//! the sufficient statistic; merges are field-wise (max/OR/min), never
//! whole-row replacement.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

use crate::common::types::WindowObservation;
use crate::db::Database;

/// Jitter tolerance when matching an observation to an open window (ms).
pub const ANCHOR_EPSILON_MS: i64 = 120_000;
/// Grace after a window's anchor passes before finalizing it (ms).
/// Shared with db::finalize_stale_windows — keep a single source of truth.
pub const FINALIZE_GRACE_MS: i64 = 180_000;
/// Activity gap threshold: consecutive events closer than this belong to one
/// active period (GA session rule; any value in the 30–60min bimodal valley
/// gives near-identical segmentation).
const ACTIVE_GAP_MS: i64 = 30 * 60_000;
/// Padding credited for an isolated event with no neighbors inside the gap.
const ISOLATED_EVENT_PAD_MS: u64 = 5 * 60_000;
/// Minimum interval between persisted writes for the same window unless the
/// integer percent changed or the window finalizes (write-rate cap, §3.6-2).
const MIN_WRITE_INTERVAL_MS: i64 = 300_000;

/// Window kind, derived from window_minutes. Session ≈ 5h, weekly ≈ 7d.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Session = 0,
    Weekly = 1,
}

impl WindowKind {
    pub fn from_minutes(minutes: u32) -> WindowKind {
        if minutes <= 24 * 60 {
            WindowKind::Session
        } else {
            WindowKind::Weekly
        }
    }

    pub fn from_u8(v: u8) -> Option<WindowKind> {
        match v {
            0 => Some(WindowKind::Session),
            1 => Some(WindowKind::Weekly),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            WindowKind::Session => "session",
            WindowKind::Weekly => "weekly",
        }
    }
}

/// How the limit boundary was experienced, if reached.
pub const REACHED_NONE: u8 = 0;
pub const REACHED_HARD_STOP: u8 = 1;
pub const REACHED_ON_CREDITS: u8 = 2;

/// On-disk value for one window instance. Serialized as
/// `[WINDOW_VALUE_VERSION u8][bincode]`. **Field order is load-bearing** for
/// bincode compatibility; append-only via a version bump.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowSnapshotV1 {
    /// Highest observed utilization, percent × 100 (e.g. 4250 = 42.5%).
    /// The statistics value — monotone by construction.
    pub peak_pct_x100: u16,
    /// Latest raw utilization, percent × 100. The live-display value: unlike
    /// the peak it can decrease (server-side limit resets, credit refills).
    pub last_pct_x100: u16,
    /// Timestamp of the newest observation merged into this row.
    pub observed_ts_ms: i64,
    /// Latest raw resets_at seen (the anchor in the key never moves; this does).
    pub raw_resets_at_ms: i64,
    /// First observation that opened this window.
    pub first_seen_ms: i64,
    pub window_minutes: u32,
    /// True once the reset time passed and the row was closed out.
    pub finalized: bool,
    pub maxed_out: bool,
    pub limit_reached_kind: u8,
    /// Time from (anchor − window length) to the first 100% sample; -1 = never.
    pub time_to_100_ms: i64,
    /// Active use overlapping this window, from the 30-min-gap rule. Lower
    /// bound: in-memory accumulation restarts with the daemon.
    pub active_ms: u64,
    /// Gap between the last sample and the (raw) reset time; large values mean
    /// the recorded peak is a lower bound (e.g. machine was asleep at reset).
    pub last_sample_gap_ms: i64,
    /// Fraction (×1000) of the window's active time that had a sample nearby.
    /// Currently constant 1000 on every path: passive extraction rides on
    /// token events by construction, and the active poller samples at ≤180s
    /// while active. The row's real coverage signal is last_sample_gap_ms;
    /// this field is reserved for a future finer-grained computation.
    pub sampled_active_fraction: u16,
    pub n_samples: u32,
    /// Inline strings, deliberately NOT dictionary ids: dict GC only scans the
    /// events keyspace and would reclaim ids referenced solely from here.
    pub limit_id: String,
    pub plan: String,
    /// Privacy-safe account scope (hex of xxh3 of the provider account id).
    pub account: String,
}

pub const WINDOW_VALUE_VERSION: u8 = 1;

/// Outcome of decoding a stored window value (see decode_versioned).
pub enum WindowDecode {
    Valid(WindowSnapshotV1),
    FutureVersion,
    Corrupt,
}

impl WindowSnapshotV1 {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(96);
        out.push(WINDOW_VALUE_VERSION);
        let body = bincode::serialize(self).expect("WindowSnapshotV1 serialization failed");
        out.extend_from_slice(&body);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<WindowSnapshotV1> {
        match Self::decode_versioned(bytes) {
            WindowDecode::Valid(s) => Some(s),
            _ => None,
        }
    }

    /// Distinguishes a FUTURE version (preserve — a newer binary wrote it)
    /// from a CORRUPT current-version value (recover — let fresh data replace
    /// it; preserving corruption would hide the row from queries forever).
    pub fn decode_versioned(bytes: &[u8]) -> WindowDecode {
        let Some((&version, body)) = bytes.split_first() else {
            return WindowDecode::Corrupt;
        };
        if version > WINDOW_VALUE_VERSION {
            return WindowDecode::FutureVersion;
        }
        match bincode::deserialize(body) {
            Ok(s) => WindowDecode::Valid(s),
            Err(_) => WindowDecode::Corrupt,
        }
    }

    /// Field-wise merge with a newer/other snapshot of the same window. Never
    /// whole-row LWW: peaks are max, flags are OR, first timestamps are min.
    pub fn merge_from(&mut self, other: &WindowSnapshotV1) {
        self.peak_pct_x100 = self.peak_pct_x100.max(other.peak_pct_x100);
        // >= not >: writes for the same observation instant apply in arrival
        // order, and the later write carries the later state.
        if other.observed_ts_ms >= self.observed_ts_ms {
            self.observed_ts_ms = other.observed_ts_ms;
            self.raw_resets_at_ms = other.raw_resets_at_ms;
            self.last_sample_gap_ms = other.last_sample_gap_ms;
            self.last_pct_x100 = other.last_pct_x100;
            // Travels with raw_resets_at_ms: a provider changing its window
            // length would otherwise leave the first writer's duration
            // against a later writer's reset (it feeds time_to_100 and the
            // active_ms clamp).
            self.window_minutes = other.window_minutes;
            self.plan = other.plan.clone();
        }
        self.first_seen_ms = self.first_seen_ms.min(other.first_seen_ms);
        self.finalized |= other.finalized;
        self.maxed_out |= other.maxed_out;
        self.limit_reached_kind = self.limit_reached_kind.max(other.limit_reached_kind);
        self.time_to_100_ms = match (self.time_to_100_ms, other.time_to_100_ms) {
            (-1, t) | (t, -1) => t,
            (a, b) => a.min(b),
        };
        self.active_ms = self.active_ms.max(other.active_ms);
        self.sampled_active_fraction = self.sampled_active_fraction.max(other.sampled_active_fraction);
        self.n_samples = self.n_samples.max(other.n_samples);
    }
}

/// Storage key: `[kind u8][limit_id_hash u64 BE][account_hash u64 BE][anchor_min_ms i64 BE]`.
/// The provider is implicit (one DB per provider). Fixed-width hashes keep the
/// key independent of the dictionary (see WindowSnapshotV1::limit_id).
pub fn window_key(kind: WindowKind, limit_id_hash: u64, account_hash: u64, anchor_min_ms: i64) -> [u8; 25] {
    let mut key = [0u8; 25];
    key[0] = kind as u8;
    key[1..9].copy_from_slice(&limit_id_hash.to_be_bytes());
    key[9..17].copy_from_slice(&account_hash.to_be_bytes());
    key[17..25].copy_from_slice(&anchor_min_ms.to_be_bytes());
    key
}

/// Anchor timestamp from the key (for retention sweeps / queries).
pub fn window_key_anchor_ms(key: &[u8]) -> Option<i64> {
    if key.len() != 25 {
        return None;
    }
    Some(i64::from_be_bytes(key[17..25].try_into().ok()?))
}

pub fn floor_to_minute(ts_ms: i64) -> i64 {
    ts_ms - ts_ms.rem_euclid(60_000)
}

pub fn hash_str(s: &str) -> u64 {
    xxh3_64(s.as_bytes())
}

/// Query/UDS output row for one window instance — the versioned public shape
/// (`schema: 1` at the response level). Field names are the wire contract for
/// the WINDOWS command, `toki query windows`, and the monitor's decoder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRow {
    pub kind: String,
    pub limit_id: String,
    pub account: String,
    pub window_end_ms: i64,
    pub raw_resets_at_ms: i64,
    pub window_minutes: u32,
    pub peak_pct: f64,
    pub last_pct: f64,
    pub observed_ts_ms: i64,
    pub first_seen_ms: i64,
    pub finalized: bool,
    pub maxed_out: bool,
    pub limit_reached_kind: u8,
    pub time_to_100_ms: i64,
    pub active_ms: u64,
    pub last_sample_gap_ms: i64,
    pub sampled_active_fraction: u16,
    pub n_samples: u32,
    pub plan: String,
}

impl WindowRow {
    pub fn from_stored(key: &[u8], snap: &WindowSnapshotV1) -> WindowRow {
        WindowRow {
            kind: key
                .first()
                .and_then(|&b| WindowKind::from_u8(b))
                .map(|k| k.label().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            limit_id: snap.limit_id.clone(),
            account: snap.account.clone(),
            window_end_ms: window_key_anchor_ms(key).unwrap_or(0),
            raw_resets_at_ms: snap.raw_resets_at_ms,
            window_minutes: snap.window_minutes,
            peak_pct: (snap.peak_pct_x100 as f64) / 100.0,
            last_pct: (snap.last_pct_x100 as f64) / 100.0,
            observed_ts_ms: snap.observed_ts_ms,
            first_seen_ms: snap.first_seen_ms,
            finalized: snap.finalized,
            maxed_out: snap.maxed_out,
            limit_reached_kind: snap.limit_reached_kind,
            time_to_100_ms: snap.time_to_100_ms,
            active_ms: snap.active_ms,
            last_sample_gap_ms: snap.last_sample_gap_ms,
            sampled_active_fraction: snap.sampled_active_fraction,
            n_samples: snap.n_samples,
            plan: snap.plan.clone(),
        }
    }
}

/// Convert a stored window row into the sync wire shape.
pub fn wire_from_stored(key: &[u8], snap: &WindowSnapshotV1) -> toki_sync_protocol::WireWindow {
    toki_sync_protocol::WireWindow {
        window_kind: key.first().copied().unwrap_or(0),
        limit_id: snap.limit_id.clone(),
        account: snap.account.clone(),
        window_end_ms: window_key_anchor_ms(key).unwrap_or(0),
        raw_resets_at_ms: snap.raw_resets_at_ms,
        window_minutes: snap.window_minutes,
        peak_pct_x100: snap.peak_pct_x100,
        last_pct_x100: snap.last_pct_x100,
        observed_ts_ms: snap.observed_ts_ms,
        first_seen_ms: snap.first_seen_ms,
        finalized: snap.finalized,
        maxed_out: snap.maxed_out,
        limit_reached_kind: snap.limit_reached_kind,
        time_to_100_ms: snap.time_to_100_ms,
        active_ms: snap.active_ms,
        last_sample_gap_ms: snap.last_sample_gap_ms,
        sampled_active_fraction: snap.sampled_active_fraction,
        n_samples: snap.n_samples,
        plan: snap.plan.clone(),
    }
}

/// A pending durable write produced by the tracker.
#[derive(Debug, Clone)]
pub struct WindowWrite {
    pub key: [u8; 25],
    pub snapshot: WindowSnapshotV1,
}

struct OpenWindow {
    kind: WindowKind,
    limit_id: String,
    limit_id_hash: u64,
    account: String,
    account_hash: u64,
    anchor_min_ms: i64,
    raw_resets_at_ms: i64,
    first_seen_ms: i64,
    window_minutes: u32,
    peak_pct: f64,
    last_pct: f64,
    maxed_out: bool,
    limit_reached_kind: u8,
    time_to_100_ms: i64,
    active_ms: u64,
    n_samples: u32,
    last_sample_ts_ms: i64,
    plan: String,
    last_written_floor: i32,
    last_written_live_floor: i32,
    last_write_ts_ms: i64,
}

impl OpenWindow {
    fn to_snapshot(&self, finalized: bool) -> WindowSnapshotV1 {
        WindowSnapshotV1 {
            peak_pct_x100: (self.peak_pct.clamp(0.0, 655.0) * 100.0).round() as u16,
            last_pct_x100: (self.last_pct.clamp(0.0, 655.0) * 100.0).round() as u16,
            observed_ts_ms: self.last_sample_ts_ms,
            raw_resets_at_ms: self.raw_resets_at_ms,
            first_seen_ms: self.first_seen_ms,
            window_minutes: self.window_minutes,
            finalized,
            maxed_out: self.maxed_out,
            limit_reached_kind: self.limit_reached_kind,
            time_to_100_ms: self.time_to_100_ms,
            // Hard invariant: a window cannot be active longer than it
            // exists. Defends the stored value against any future
            // accounting bug (max-merge would make an excess permanent).
            active_ms: self.active_ms.min(self.window_minutes as u64 * 60_000),
            last_sample_gap_ms: self.raw_resets_at_ms - self.last_sample_ts_ms,
            sampled_active_fraction: 1000,
            n_samples: self.n_samples,
            limit_id: self.limit_id.clone(),
            plan: self.plan.clone(),
            account: self.account.clone(),
        }
    }

    fn key(&self) -> [u8; 25] {
        window_key(self.kind, self.limit_id_hash, self.account_hash, self.anchor_min_ms)
    }
}

/// Per-provider in-memory tracker. Owned by the engine (watch thread) — no
/// interior locking; observations arrive on the same thread that parses them.
pub struct WindowTracker {
    open: Vec<OpenWindow>,
    last_activity_ts_ms: i64,
    /// Account scope applied to new windows ("unknown" until resolved).
    account: String,
    account_hash: u64,
}

impl WindowTracker {
    pub fn new() -> Self {
        WindowTracker {
            open: Vec::new(),
            last_activity_ts_ms: 0,
            account: "unknown".to_string(),
            account_hash: hash_str("unknown"),
        }
    }

    pub fn set_account(&mut self, account_scope: &str) {
        if self.account != account_scope {
            self.account = account_scope.to_string();
            self.account_hash = hash_str(account_scope);
        }
    }

    /// Feed one token-event timestamp for active-time accounting (30-min gap rule).
    pub fn observe_activity(&mut self, ts_ms: i64) {
        if ts_ms <= 0 {
            return;
        }
        let gap = ts_ms - self.last_activity_ts_ms;
        let credit: u64 = if self.last_activity_ts_ms == 0 || gap > ACTIVE_GAP_MS {
            ISOLATED_EVENT_PAD_MS
        } else if gap > 0 {
            gap as u64
        } else {
            0
        };
        if credit > 0 {
            // ONLY the current account's windows: after a switch the previous
            // account's still-open rows must not absorb the new account's
            // activity (max-merge would make that permanent at finalize).
            let current = self.account_hash;
            for w in self.open.iter_mut().filter(|w| w.account_hash == current) {
                w.active_ms = w.active_ms.saturating_add(credit);
            }
        }
        if ts_ms > self.last_activity_ts_ms {
            self.last_activity_ts_ms = ts_ms;
        }
    }

    /// Feed one rate-limit observation. Returns a write when the persistence
    /// policy says this state is worth flushing (new window / integer percent
    /// change / MIN_WRITE_INTERVAL elapsed).
    pub fn observe(&mut self, obs: &WindowObservation) -> Option<WindowWrite> {
        if obs.resets_at_ms <= 0 || obs.ts_ms <= 0 || !obs.used_percent.is_finite() {
            return None;
        }
        // Local sanity (the server validates uploads, but nothing guarded the
        // local parser): a corrupt line with an absurd duration or a reset
        // farther away than the window's own length would open an entry that
        // never finalizes and mint a garbage storage key.
        if obs.window_minutes == 0 || obs.window_minutes > 60 * 24 * 31 {
            return None;
        }
        if obs.resets_at_ms - obs.ts_ms > (obs.window_minutes as i64) * 60_000 + 86_400_000 {
            return None;
        }
        let kind = WindowKind::from_minutes(obs.window_minutes);
        let limit_hash = hash_str(&obs.limit_id);

        // Match against an open window: same kind + limit + account, resets_at
        // within epsilon of the latest raw value AND bounded to the immutable
        // anchor (audit: matching raw alone could let repeated <=epsilon steps
        // walk arbitrarily far from the anchor; the anchor bound caps drift).
        let idx = self.open.iter().position(|w| {
            w.kind == kind
                && w.limit_id_hash == limit_hash
                && w.account_hash == self.account_hash
                && (w.raw_resets_at_ms - obs.resets_at_ms).abs() <= ANCHOR_EPSILON_MS
                && (w.anchor_min_ms - obs.resets_at_ms).abs() <= ANCHOR_EPSILON_MS + 60_000
        });

        match idx {
            Some(i) => {
                let w = &mut self.open[i];
                // Monotone merge: decreases are flicker noise or a server-side
                // reset; peak keeps the max either way.
                if obs.used_percent > w.peak_pct {
                    w.peak_pct = obs.used_percent;
                }
                // >= not >: same-millisecond observations arrive in file
                // order, and the later line is the later observation.
                if obs.ts_ms >= w.last_sample_ts_ms {
                    w.last_sample_ts_ms = obs.ts_ms;
                    w.raw_resets_at_ms = obs.resets_at_ms;
                    w.last_pct = obs.used_percent;
                    if let Some(p) = &obs.plan_type {
                        if w.plan != *p {
                            w.plan = p.clone();
                        }
                    }
                }
                w.n_samples = w.n_samples.saturating_add(1);
                if obs.used_percent >= 99.995 && !w.maxed_out {
                    w.maxed_out = true;
                    let window_start = w.anchor_min_ms - (w.window_minutes as i64) * 60_000;
                    w.time_to_100_ms = (obs.ts_ms - window_start).max(0);
                }
                if obs.limit_reached {
                    let kind_reached = if obs.has_credits { REACHED_ON_CREDITS } else { REACHED_HARD_STOP };
                    w.limit_reached_kind = w.limit_reached_kind.max(kind_reached);
                }

                // The stored row is also the monitor's LIVE source: the raw
                // percentage (last_pct) must flush on integer change too, or
                // the widget lags up to MIN_WRITE_INTERVAL behind (peak alone
                // is monotone and can sit still for the whole interval).
                let floor = w.peak_pct.floor() as i32;
                let live_floor = w.last_pct.floor() as i32;
                let due = floor != w.last_written_floor
                    || live_floor != w.last_written_live_floor
                    || obs.ts_ms - w.last_write_ts_ms >= MIN_WRITE_INTERVAL_MS;
                if due {
                    let w = &mut self.open[i];
                    w.last_written_floor = floor;
                    w.last_written_live_floor = live_floor;
                    w.last_write_ts_ms = obs.ts_ms;
                    let snap = w.to_snapshot(false);
                    let key = w.key();
                    Some(WindowWrite { key, snapshot: snap })
                } else {
                    None
                }
            }
            None => {
                // Passively-extracted unused limits slide resets_at
                // continuously (Phase 0: ~130s steps), so a window is only
                // born from a >0% observation — unless the source vouches for
                // anchor stability (Claude's active poller: a served
                // resets_at is a real window even at 0%, and genuine zero-use
                // weekly periods must exist for the overall mean).
                // The exception exists so genuine zero-use WEEKLY periods
                // enter the overall mean. A synthetic 0% session window is not
                // that: it would add an untouched row to the 5h duty-cycle and
                // active-mean statistics, and it is the shape most at risk if
                // a provider ever slides an unused window's resets_at.
                if obs.used_percent <= 0.0
                    && !(obs.anchor_stable && kind == WindowKind::Weekly)
                {
                    return None;
                }
                // A resets_at already past is a stale replay: it may only
                // finalize an existing row, never open a new one.
                //
                // The tolerance must stay BELOW `FINALIZE_GRACE_MS − max
                // observed jitter (93s)`, or a late sample carrying a
                // positively-jittered reset slips between the close cutoff and
                // the reopen cutoff and mints a ghost row at a second anchor
                // that nothing ever merges away. 60s satisfies that
                // (93 + 60 < 180) while still tolerating clock skew.
                const REOPEN_TOLERANCE_MS: i64 = 60_000;
                if obs.resets_at_ms + REOPEN_TOLERANCE_MS < obs.ts_ms {
                    return None;
                }
                let w = OpenWindow {
                    kind,
                    limit_id: obs.limit_id.clone(),
                    limit_id_hash: limit_hash,
                    account: self.account.clone(),
                    account_hash: self.account_hash,
                    anchor_min_ms: floor_to_minute(obs.resets_at_ms),
                    raw_resets_at_ms: obs.resets_at_ms,
                    first_seen_ms: obs.ts_ms,
                    window_minutes: obs.window_minutes,
                    peak_pct: obs.used_percent,
                    last_pct: obs.used_percent,
                    maxed_out: obs.used_percent >= 99.995,
                    limit_reached_kind: if obs.limit_reached {
                        if obs.has_credits { REACHED_ON_CREDITS } else { REACHED_HARD_STOP }
                    } else {
                        REACHED_NONE
                    },
                    time_to_100_ms: -1,
                    active_ms: 0,
                    n_samples: 1,
                    last_sample_ts_ms: obs.ts_ms,
                    plan: obs.plan_type.clone().unwrap_or_default(),
                    last_written_floor: obs.used_percent.floor() as i32,
                    last_written_live_floor: obs.used_percent.floor() as i32,
                    last_write_ts_ms: obs.ts_ms,
                };
                if w.maxed_out {
                    // Opened already at 100%: earliest bound we can state.
                    let window_start = w.anchor_min_ms - (w.window_minutes as i64) * 60_000;
                    let mut w = w;
                    w.time_to_100_ms = (obs.ts_ms - window_start).max(0);
                    let write = WindowWrite { key: w.key(), snapshot: w.to_snapshot(false) };
                    self.open.push(w);
                    return Some(write);
                }
                let write = WindowWrite { key: w.key(), snapshot: w.to_snapshot(false) };
                self.open.push(w);
                Some(write)
            }
        }
    }

    /// Account scope currently applied to new windows.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Raw reset instants of currently open windows (poller confirm scheduling).
    pub fn open_reset_times(&self) -> Vec<i64> {
        self.open.iter().map(|w| w.raw_resets_at_ms).collect()
    }

    /// Close out windows whose reset time has passed. Returns their final writes.
    pub fn finalize_expired(&mut self, now_ms: i64) -> Vec<WindowWrite> {
        let mut writes = Vec::new();
        self.open.retain(|w| {
            if now_ms > w.raw_resets_at_ms + FINALIZE_GRACE_MS {
                writes.push(WindowWrite { key: w.key(), snapshot: w.to_snapshot(true) });
                false
            } else {
                true
            }
        });
        writes
    }

    /// Flush all open windows without finalizing (daemon shutdown).
    pub fn flush_all(&mut self) -> Vec<WindowWrite> {
        self.open
            .iter()
            .map(|w| WindowWrite { key: w.key(), snapshot: w.to_snapshot(false) })
            .collect()
    }
}

/// Resolve the Codex account scope. Kept as a thin delegate: provider
/// specifics live in providers::codex (established home for provider I/O).
pub fn codex_account_scope(codex_root: &str) -> String {
    crate::providers::codex::account_scope(codex_root)
}

/// mtime-cached account scope for periodic callers: auth.json only changes on
/// login/logout, so the 60s engine tick shouldn't re-read and re-parse it.
pub struct CachedAccountScope {
    root: String,
    mtime: Option<std::time::SystemTime>,
    scope: String,
}

impl CachedAccountScope {
    pub fn new(root: String) -> Self {
        CachedAccountScope { root, mtime: None, scope: "unknown".to_string() }
    }

    pub fn resolve(&mut self) -> &str {
        let path = std::path::Path::new(&self.root).join("auth.json");
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        // Equality alone: an absent file stays None==None and keeps the
        // cached "unknown" (an `is_none()` re-check re-read the file on
        // every call — exactly the cost this cache exists to avoid).
        if mtime != self.mtime {
            self.mtime = mtime;
            self.scope = codex_account_scope(&self.root);
        }
        &self.scope
    }
}

/// One-time-deep, thereafter-shallow startup scan over Codex rollout files that
/// replays historical `rate_limits` into window rows. Idempotent by design
/// (field-wise merge), so overlapping rescans are safe. Runs on its own thread,
/// throttled, after cold start — deliberately outside the checkpointed
/// cold-start path, which never revisits already-consumed files.
pub fn run_windows_backfill(
    db: Arc<Database>,
    db_tx: crossbeam_channel::Sender<crate::writer::DbOp>,
    sessions_glob: String,
    account: String,
) {
    const FIRST_RUN_DAYS: i64 = 60;
    const CATCHUP_DAYS: i64 = 8;
    const MARKER_KEY: &str = "windows_scan_ms";

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let last_scan_ms: i64 = db
        .get_setting(MARKER_KEY)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // Catch-up covers max(8d, time since the last completed scan + 1d overlap),
    // capped at the first-run depth — a daemon down for >8 days must not
    // permanently lose the gap.
    let lookback_ms = if last_scan_ms == 0 {
        FIRST_RUN_DAYS * 86_400_000
    } else {
        (now_ms - last_scan_ms + 86_400_000)
            .max(CATCHUP_DAYS * 86_400_000)
            .min(FIRST_RUN_DAYS * 86_400_000)
    };
    let cutoff_ms = now_ms - lookback_ms;
    // First run replays months of history that cannot be attributed to the
    // current login with confidence; catch-up scans cover recent days only.
    let first_run = last_scan_ms == 0;
    let scan_account = if first_run { "unknown".to_string() } else { account };

    let mut tracker = WindowTracker::new();
    tracker.set_account(&scan_account);
    let mut files_scanned = 0u32;
    let mut io_errors = 0u32;

    let mut paths: Vec<std::path::PathBuf> = glob::glob(&sessions_glob)
        .into_iter()
        .flatten()
        .filter_map(|p| p.ok())
        .filter(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| (d.as_millis() as i64) >= cutoff_ms)
                .unwrap_or(false)
        })
        .collect();
    paths.sort(); // filename order == chronological order for rollout files

    // Collect observations from every file FIRST, then replay in global
    // timestamp order. Rollout sessions overlap in wall-clock time (a long
    // session spans files that sort after it), so a per-file replay drives
    // the tracker's clock BACKWARDS at each file boundary: activity gaps go
    // negative (credit 0 — measured ~25% of historical active time lost) and
    // finalize_expired closes windows that a later-read, earlier-timestamped
    // file then re-opens under a second anchor. Only the observations are
    // buffered (a few thousand), never the file text.
    let mut observations: Vec<crate::common::types::WindowObservation> = Vec::new();
    for path in paths {
        let path_str = path.to_string_lossy();
        // Zero-alloc line reader (mmap + memchr) — the same one the watch hot
        // path uses; BufReader::lines() heap-allocated a String per line
        // across 60 days of history.
        let scan = crate::checkpoint::process_lines_streaming(&path_str, 0, |line| {
            if !line.contains("\"rate_limits\"") {
                return;
            }
            let Some(obs) = crate::providers::codex::parse_rate_limits_line(line) else {
                return;
            };
            observations.extend([obs.primary, obs.secondary].into_iter().flatten());
        });
        if scan.is_err() {
            io_errors += 1;
            continue;
        }
        files_scanned += 1;
        // Deliberate throttle: this is background work, never a startup burst.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    observations.sort_by_key(|o| o.ts_ms);

    let mut writes: Vec<WindowWrite> = Vec::new();
    let mut idx = 0usize;
    while idx < observations.len() {
        let line_ts = observations[idx].ts_ms;
        // Replay time only moves forward: close what this instant expires...
        writes.extend(tracker.finalize_expired(line_ts));
        // ...open every slot observed at this instant...
        let start = idx;
        while idx < observations.len() && observations[idx].ts_ms == line_ts {
            let _ = tracker.observe(&observations[idx]);
            idx += 1;
        }
        // ...then credit the instant's activity ONCE (rate_limits rides
        // token_count lines, so one instant is one token-activity event).
        let _ = start;
        tracker.observe_activity(line_ts);
    }
    writes.extend(tracker.finalize_expired(now_ms));
    // On the first run the scan is attributed to "unknown", so any window
    // still OPEN right now would be written a second time under a different
    // account key — the live tracker already owns those. Emit only closed
    // windows; catch-up runs (real account) flush normally.
    if !first_run {
        writes.extend(tracker.flush_all());
    }

    // All window writes flow through the writer thread: upsert_window_merge
    // is read-merge-write, and a direct write here could race a concurrent
    // live-collection write for the same window (audit finding).
    let n = writes.len();
    for w in writes {
        if db_tx.send(crate::writer::DbOp::WriteWindow(Box::new(w))).is_err() {
            eprintln!("[toki] windows backfill: writer channel closed");
            return; // leave marker unset so the next start retries
        }
    }
    // Barrier: the writer processes its channel FIFO, so an acked flush op
    // proves every window write above has been applied before the marker.
    let (ack_tx, ack_rx) = crossbeam_channel::bounded::<()>(1);
    if db_tx.send(crate::writer::DbOp::FlushBulkEvents(ack_tx)).is_err()
        || ack_rx.recv_timeout(std::time::Duration::from_secs(60)).is_err()
    {
        eprintln!("[toki] windows backfill: flush barrier failed; marker not set");
        return;
    }
    // Partial I/O failure leaves the marker untouched so the next start
    // rescans (idempotent merge makes the overlap free).
    if io_errors > 0 {
        eprintln!("[toki] windows backfill: {io_errors} files unreadable; marker not advanced");
        return;
    }
    let _ = db.set_setting(MARKER_KEY, &now_ms.to_string());
    if n > 0 {
        eprintln!(
            "[toki] windows backfill: {} files scanned, {} snapshots merged",
            files_scanned, n
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(limit: &str, minutes: u32, pct: f64, resets_at_ms: i64, ts_ms: i64) -> WindowObservation {
        WindowObservation {
            limit_id: limit.to_string(),
            window_minutes: minutes,
            used_percent: pct,
            resets_at_ms,
            plan_type: Some("prolite".to_string()),
            limit_reached: false,
            has_credits: false,
            anchor_stable: false,
            ts_ms,
        }
    }

    /// Golden-bytes fixture: decodes a byte string captured from the v1
    /// encoder. Length checks alone cannot catch same-width field swaps
    /// (two i64s exchanged still round-trips); this pins the actual layout.
    /// If it fails you broke on-disk compatibility — bump
    /// WINDOW_VALUE_VERSION instead of editing fields.
    #[test]
    fn window_snapshot_v1_golden_bytes_decode() {
        let snap = WindowSnapshotV1 {
            peak_pct_x100: 0x1111,
            last_pct_x100: 0x2222,
            observed_ts_ms: 0x3333,
            raw_resets_at_ms: 0x4444,
            first_seen_ms: 0x5555,
            window_minutes: 0x6666,
            finalized: true,
            maxed_out: false,
            limit_reached_kind: 2,
            time_to_100_ms: -1,
            active_ms: 0x7777,
            last_sample_gap_ms: 0x8888,
            sampled_active_fraction: 0x9999,
            n_samples: 0xAAAA,
            limit_id: "L".into(),
            plan: "P".into(),
            account: "A".into(),
        };
        let bytes = snap.encode();
        // Distinct sentinel values pin each field's byte position: a swap of
        // any two fields (even same-width) changes this fingerprint.
        let expected: Vec<u8> = {
            let mut v = vec![WINDOW_VALUE_VERSION];
            v.extend_from_slice(&0x1111u16.to_le_bytes());
            v.extend_from_slice(&0x2222u16.to_le_bytes());
            v.extend_from_slice(&0x3333i64.to_le_bytes());
            v.extend_from_slice(&0x4444i64.to_le_bytes());
            v.extend_from_slice(&0x5555i64.to_le_bytes());
            v.extend_from_slice(&0x6666u32.to_le_bytes());
            v.push(1); // finalized
            v.push(0); // maxed_out
            v.push(2); // limit_reached_kind
            v.extend_from_slice(&(-1i64).to_le_bytes());
            v.extend_from_slice(&0x7777u64.to_le_bytes());
            v.extend_from_slice(&0x8888i64.to_le_bytes());
            v.extend_from_slice(&0x9999u16.to_le_bytes());
            v.extend_from_slice(&0xAAAAu32.to_le_bytes());
            for st in ["L", "P", "A"] {
                v.extend_from_slice(&(st.len() as u64).to_le_bytes());
                v.extend_from_slice(st.as_bytes());
            }
            v
        };
        assert_eq!(bytes, expected, "v1 byte layout changed — bump WINDOW_VALUE_VERSION");
        assert_eq!(WindowSnapshotV1::decode(&expected).unwrap(), snap);
    }

    /// Guards the bincode layout of the on-disk value. If this fails you broke
    /// compatibility — bump WINDOW_VALUE_VERSION instead of editing fields.
    #[test]
    fn window_snapshot_field_order_stable() {
        let snap = WindowSnapshotV1 {
            peak_pct_x100: 4250,
            last_pct_x100: 4250,
            observed_ts_ms: 1_786_000_000_000,
            raw_resets_at_ms: 1_786_000_060_000,
            first_seen_ms: 1_785_999_000_000,
            window_minutes: 300,
            finalized: true,
            maxed_out: false,
            limit_reached_kind: REACHED_NONE,
            time_to_100_ms: -1,
            active_ms: 3_600_000,
            last_sample_gap_ms: 60_000,
            sampled_active_fraction: 1000,
            n_samples: 42,
            limit_id: "codex".into(),
            plan: "prolite".into(),
            account: "abcd".into(),
        };
        let enc = snap.encode();
        assert_eq!(enc[0], WINDOW_VALUE_VERSION);
        let dec = WindowSnapshotV1::decode(&enc).unwrap();
        assert_eq!(dec, snap);
        // Layout fingerprint: version(1) + fixed fields + 3 length-prefixed strings.
        let fixed = 2 + 2 + 8 + 8 + 8 + 4 + 1 + 1 + 1 + 8 + 8 + 8 + 2 + 4;
        let strings = (8 + 5) + (8 + 7) + (8 + 4);
        assert_eq!(enc.len(), 1 + fixed + strings);
    }

    #[test]
    fn zero_percent_never_opens_window() {
        let mut t = WindowTracker::new();
        // Sliding anchors of an unused limit (observed: ~130s steps).
        assert!(t.observe(&obs("codex_bengalfox", 10080, 0.0, 1_784_684_645_000, 1_784_080_000_000)).is_none());
        assert!(t.observe(&obs("codex_bengalfox", 10080, 0.0, 1_784_684_773_000, 1_784_080_130_000)).is_none());
        assert!(t.open.is_empty());
    }

    #[test]
    fn jittered_resets_merge_into_one_window() {
        let mut t = WindowTracker::new();
        let base_reset = 1_784_524_231_000;
        let w1 = t.observe(&obs("codex", 10080, 3.0, base_reset, 1_784_000_000_000));
        assert!(w1.is_some());
        // +1s jitter (observed in real data) merges; anchor unchanged.
        let _ = t.observe(&obs("codex", 10080, 5.0, base_reset + 1_000, 1_784_000_600_000));
        let _ = t.observe(&obs("codex", 10080, 7.0, base_reset - 15_000, 1_784_001_200_000));
        assert_eq!(t.open.len(), 1);
        assert_eq!(t.open[0].anchor_min_ms, floor_to_minute(base_reset));
        assert_eq!(t.open[0].peak_pct, 7.0);
    }

    #[test]
    fn separate_limit_ids_are_separate_windows() {
        let mut t = WindowTracker::new();
        let _ = t.observe(&obs("codex", 10080, 17.0, 1_782_717_082_000, 1_782_100_000_000));
        let _ = t.observe(&obs("codex_gpt5", 10080, 1.0, 1_782_717_082_000, 1_782_100_001_000));
        assert_eq!(t.open.len(), 2);
    }

    #[test]
    fn peak_survives_server_side_reset_to_zero() {
        // Real case 2026-03-22: 26% → 0% under the same anchor.
        let mut t = WindowTracker::new();
        let reset = 1_774_793_876_000;
        let _ = t.observe(&obs("codex", 10080, 26.0, reset, 1_774_200_000_000));
        let _ = t.observe(&obs("codex", 10080, 0.0, reset, 1_774_226_000_000));
        assert_eq!(t.open.len(), 1);
        assert_eq!(t.open[0].peak_pct, 26.0);
    }

    #[test]
    fn write_policy_caps_frequency() {
        let mut t = WindowTracker::new();
        let reset = 1_786_000_000_000;
        let t0 = reset - 4 * 3_600_000; // inside the 5h window
        assert!(t.observe(&obs("codex", 300, 10.0, reset, t0)).is_some()); // new window
        // Sub-integer wiggle within the interval: suppressed.
        assert!(t.observe(&obs("codex", 300, 10.4, reset, t0 + 10_000)).is_none());
        // Integer boundary crossed: written.
        assert!(t.observe(&obs("codex", 300, 11.0, reset, t0 + 20_000)).is_some());
        // No change, but MIN_WRITE_INTERVAL elapsed: heartbeat write.
        assert!(t.observe(&obs("codex", 300, 11.2, reset, t0 + 20_000 + MIN_WRITE_INTERVAL_MS)).is_some());
    }

    #[test]
    fn maxed_out_records_time_to_100() {
        let mut t = WindowTracker::new();
        let reset = 1_786_000_000_000i64;
        let window_start = floor_to_minute(reset) - 300 * 60_000;
        let hit_ts = window_start + 2 * 3_600_000; // maxed after 2h of a 5h window
        let _ = t.observe(&obs("codex", 300, 50.0, reset, window_start + 3_600_000));
        let _ = t.observe(&obs("codex", 300, 100.0, reset, hit_ts));
        assert!(t.open[0].maxed_out);
        assert_eq!(t.open[0].time_to_100_ms, 2 * 3_600_000);
    }

    #[test]
    fn finalize_expired_closes_and_emits() {
        let mut t = WindowTracker::new();
        let reset = 1_786_000_000_000i64;
        let _ = t.observe(&obs("codex", 300, 42.0, reset, reset - 3_600_000));
        let writes = t.finalize_expired(reset + FINALIZE_GRACE_MS + 1);
        assert_eq!(writes.len(), 1);
        assert!(writes[0].snapshot.finalized);
        assert_eq!(writes[0].snapshot.peak_pct_x100, 4200);
        assert!(t.open.is_empty());
    }

    #[test]
    fn anchor_walk_is_bounded() {
        // Repeated <=epsilon steps must not drift a window arbitrarily far
        // from its immutable anchor (audit finding: raw-relative matching
        // alone allows an unbounded walk).
        let mut t = WindowTracker::new();
        let base = 1_786_000_000_000i64;
        let _ = t.observe(&obs("codex", 300, 10.0, base, base - 3_600_000));
        // Walk in +100s steps: each within epsilon of the previous raw, but
        // step 3 exceeds the anchor bound and must open a NEW window.
        let _ = t.observe(&obs("codex", 300, 11.0, base + 100_000, base - 3_500_000));
        let _ = t.observe(&obs("codex", 300, 12.0, base + 200_000, base - 3_400_000));
        let _ = t.observe(&obs("codex", 300, 13.0, base + 300_000, base - 3_300_000));
        assert!(t.open.len() >= 2, "walk past the anchor bound must split");
        assert_eq!(t.open[0].anchor_min_ms, floor_to_minute(base));
    }

    #[test]
    fn account_change_never_merges_into_old_row() {
        let mut t = WindowTracker::new();
        t.set_account("acct-a");
        let reset = 1_786_000_000_000i64;
        let _ = t.observe(&obs("codex", 300, 10.0, reset, reset - 3_600_000));
        t.set_account("acct-b");
        let _ = t.observe(&obs("codex", 300, 3.0, reset, reset - 3_000_000));
        assert_eq!(t.open.len(), 2);
        assert_ne!(t.open[0].account_hash, t.open[1].account_hash);
    }

    #[test]
    fn zero_percent_session_never_opens_even_when_anchor_stable() {
        // The exception is for genuine zero-use WEEKLY periods; a synthetic
        // 0% session row would pollute the 5h duty-cycle statistics.
        let mut t = WindowTracker::new();
        let mut o = obs("five_hour", 300, 0.0, 1_786_000_000_000, 1_785_999_000_000);
        o.anchor_stable = true;
        assert!(t.observe(&o).is_none());
        assert!(t.open.is_empty());
    }

    #[test]
    fn late_jittered_sample_cannot_mint_a_ghost_row() {
        // A sample arriving after finalize, with a positively-jittered reset,
        // used to slip between the epsilon (120s) and the grace (180s) and
        // open a second anchor for a window that was already closed.
        let mut t = WindowTracker::new();
        let reset = 1_786_000_000_000i64;
        let _ = t.observe(&obs("codex", 300, 40.0, reset, reset - 3_600_000));
        let closed = t.finalize_expired(reset + FINALIZE_GRACE_MS + 1);
        assert_eq!(closed.len(), 1);
        // ts is past reset+grace; a +90s jittered reset must NOT reopen.
        let late = obs("codex", 300, 41.0, reset + 90_000, reset + FINALIZE_GRACE_MS + 30_000);
        assert!(t.observe(&late).is_none());
        assert!(t.open.is_empty());
    }

    #[test]
    fn zero_percent_opens_window_when_anchor_stable() {
        // Claude's active poller vouches for the anchor: a genuine zero-use
        // weekly window must exist (weekly overall mean would otherwise bias
        // toward active weeks).
        let mut t = WindowTracker::new();
        let mut o = obs("seven_day", 10_080, 0.0, 1_786_000_000_000, 1_785_500_000_000);
        o.anchor_stable = true; // weekly: the documented exception
        assert!(t.observe(&o).is_some());
        assert_eq!(t.open.len(), 1);
        assert_eq!(t.open[0].peak_pct, 0.0);
    }

    #[test]
    fn last_pct_tracks_latest_not_peak() {
        let mut t = WindowTracker::new();
        let reset = 1_774_793_876_000i64;
        let _ = t.observe(&obs("codex", 10080, 26.0, reset, 1_774_200_000_000));
        let _ = t.observe(&obs("codex", 10080, 0.0, reset, 1_774_226_000_000));
        let w = &t.open[0];
        assert_eq!(w.peak_pct, 26.0); // statistics keep the max
        assert_eq!(w.last_pct, 0.0); // live display follows the raw value
    }

    #[test]
    fn implausible_observations_are_rejected() {
        let mut t = WindowTracker::new();
        // Reset farther in the future than the window's own length: corrupt.
        let far = obs("codex", 300, 10.0, 1_786_000_000_000 + 3 * 86_400_000, 1_786_000_000_000);
        assert!(t.observe(&far).is_none());
        // Zero / absurd durations.
        assert!(t.observe(&obs("codex", 0, 10.0, 1_786_000_000_000, 1_785_999_000_000)).is_none());
        assert!(t.observe(&obs("codex", 60 * 24 * 366, 10.0, 1_786_000_000_000, 1_785_999_000_000)).is_none());
        assert!(t.open.is_empty());
    }

    #[test]
    fn stale_past_resets_never_open_windows() {
        let mut t = WindowTracker::new();
        // Observation timestamped long after its own reset time (log replay).
        assert!(t.observe(&obs("codex", 300, 40.0, 1_780_000_000_000, 1_780_010_000_000)).is_none());
        assert!(t.open.is_empty());
    }

    #[test]
    fn activity_accumulates_with_gap_rule() {
        let mut t = WindowTracker::new();
        let reset = 1_786_000_000_000i64;
        let t0 = reset - 4 * 3_600_000;
        let _ = t.observe(&obs("codex", 300, 5.0, reset, t0));
        t.observe_activity(t0); // isolated: pad
        t.observe_activity(t0 + 60_000); // +60s gap
        t.observe_activity(t0 + 120_000); // +60s gap
        t.observe_activity(t0 + 120_000 + ACTIVE_GAP_MS + 1); // over gap: pad
        let w = &t.open[0];
        assert_eq!(w.active_ms, ISOLATED_EVENT_PAD_MS + 60_000 + 60_000 + ISOLATED_EVENT_PAD_MS);
    }

    #[test]
    fn merge_is_field_wise_not_row_lww() {
        let mut a = WindowSnapshotV1 {
            peak_pct_x100: 9000,
            last_pct_x100: 9000,
            observed_ts_ms: 100,
            raw_resets_at_ms: 1_000,
            first_seen_ms: 50,
            window_minutes: 300,
            finalized: false,
            maxed_out: false,
            limit_reached_kind: REACHED_NONE,
            time_to_100_ms: -1,
            active_ms: 500,
            last_sample_gap_ms: 900,
            sampled_active_fraction: 1000,
            n_samples: 10,
            limit_id: "codex".into(),
            plan: "old".into(),
            account: "x".into(),
        };
        let b = WindowSnapshotV1 {
            peak_pct_x100: 4000, // older device saw lower peak
            last_pct_x100: 400,  // raw dropped to 4% post server-reset
            observed_ts_ms: 200, // but observed later (post server-reset)
            raw_resets_at_ms: 1_060,
            first_seen_ms: 80,
            window_minutes: 300,
            finalized: true,
            maxed_out: true,
            limit_reached_kind: REACHED_HARD_STOP,
            time_to_100_ms: 7_200_000,
            active_ms: 300,
            last_sample_gap_ms: 860,
            sampled_active_fraction: 900,
            n_samples: 4,
            limit_id: "codex".into(),
            plan: "new".into(),
            account: "x".into(),
        };
        a.merge_from(&b);
        assert_eq!(a.peak_pct_x100, 9000); // max, not latest
        assert_eq!(a.observed_ts_ms, 200); // latest observation metadata
        assert_eq!(a.plan, "new");
        assert_eq!(a.first_seen_ms, 50); // min
        assert!(a.finalized && a.maxed_out); // OR
        assert_eq!(a.time_to_100_ms, 7_200_000); // -1 loses to a real value
        assert_eq!(a.active_ms, 500); // max
    }

    /// Drift guard for the two hand-maintained field mappings in this file:
    /// WindowRow::from_stored (query/UDS shape) and wire_from_stored (sync
    /// shape) must agree field-by-field for the same stored row.
    #[test]
    fn row_and_wire_conversions_stay_in_lockstep() {
        let key = window_key(WindowKind::Weekly, 7, 9, 1_786_000_020_000);
        let snap = WindowSnapshotV1 {
            peak_pct_x100: 4321,
            last_pct_x100: 1234,
            observed_ts_ms: 11,
            raw_resets_at_ms: 22,
            first_seen_ms: 33,
            window_minutes: 10080,
            finalized: true,
            maxed_out: true,
            limit_reached_kind: REACHED_ON_CREDITS,
            time_to_100_ms: 44,
            active_ms: 55,
            last_sample_gap_ms: 66,
            sampled_active_fraction: 777,
            n_samples: 88,
            limit_id: "codex".into(),
            plan: "prolite".into(),
            account: "acct".into(),
        };
        let row = WindowRow::from_stored(&key, &snap);
        let wire = wire_from_stored(&key, &snap);
        assert_eq!(row.kind, "weekly");
        assert_eq!(wire.window_kind, 1);
        assert_eq!(row.window_end_ms, wire.window_end_ms);
        assert_eq!((row.peak_pct * 100.0).round() as u16, wire.peak_pct_x100);
        assert_eq!((row.last_pct * 100.0).round() as u16, wire.last_pct_x100);
        assert_eq!(row.raw_resets_at_ms, wire.raw_resets_at_ms);
        assert_eq!(row.first_seen_ms, wire.first_seen_ms);
        assert_eq!(row.observed_ts_ms, wire.observed_ts_ms);
        assert_eq!(row.window_minutes, wire.window_minutes);
        assert_eq!(row.finalized, wire.finalized);
        assert_eq!(row.maxed_out, wire.maxed_out);
        assert_eq!(row.limit_reached_kind, wire.limit_reached_kind);
        assert_eq!(row.time_to_100_ms, wire.time_to_100_ms);
        assert_eq!(row.active_ms, wire.active_ms);
        assert_eq!(row.last_sample_gap_ms, wire.last_sample_gap_ms);
        assert_eq!(row.sampled_active_fraction, wire.sampled_active_fraction);
        assert_eq!(row.n_samples, wire.n_samples);
        assert_eq!(row.limit_id, wire.limit_id);
        assert_eq!(row.plan, wire.plan);
        assert_eq!(row.account, wire.account);
    }

    #[test]
    fn window_key_roundtrip_and_order() {
        let k1 = window_key(WindowKind::Session, 1, 2, 1_786_000_000_000);
        let k2 = window_key(WindowKind::Session, 1, 2, 1_786_000_060_000);
        assert!(k1 < k2); // BE anchor keeps same-window-series ordered by time
        assert_eq!(window_key_anchor_ms(&k1), Some(1_786_000_000_000));
    }
}
