//! End-to-end window sync against a REAL toki-sync server.
//!
//! Everything between the client's `sync_windows` call and the server's stored
//! row was previously untested: unit tests stop at the sender trait on one side
//! and start at the decoded frame on the other, so TCP, the auth handshake and
//! the wire framing had no coverage. This closes that gap by driving the real
//! `SyncClient` against a server in Docker.
//!
//! Skipped unless TOKI_E2E_ADDR and TOKI_E2E_JWT are set (see
//! scripts that bring the container up); `cargo test` stays hermetic.

use toki::db::Database;
use toki::windows::{window_key, WindowKind, WindowSnapshotV1, REACHED_NONE};

fn env2() -> Option<(String, String)> {
    Some((std::env::var("TOKI_E2E_ADDR").ok()?, std::env::var("TOKI_E2E_JWT").ok()?))
}

fn snap(peak: u16, anchor: i64, limit: &str) -> WindowSnapshotV1 {
    WindowSnapshotV1 {
        peak_pct_x100: peak,
        last_pct_x100: peak,
        observed_ts_ms: anchor - 1_000,
        raw_resets_at_ms: anchor,
        first_seen_ms: anchor - 300_000,
        window_minutes: 300,
        finalized: true,
        maxed_out: false,
        limit_reached_kind: REACHED_NONE,
        time_to_100_ms: -1,
        active_ms: 60_000,
        last_sample_gap_ms: 1_000,
        sampled_active_fraction: 1000,
        n_samples: 4,
        limit_id: limit.to_string(),
        plan: "max_5x".into(),
        account: "e2e-account".into(),
    }
}

#[test]
fn windows_reach_a_real_server_over_tcp() {
    let Some((addr, jwt)) = env2() else {
        eprintln!("skipping: TOKI_E2E_ADDR / TOKI_E2E_JWT not set");
        return;
    };
    // Anchors must be IDENTICAL across runs, otherwise a resend uploads
    // different logical windows and "merged in place" cannot be observed.
    let now_ms: i64 = std::env::var("TOKI_E2E_ANCHOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
        });

    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("e2e.fjall")).unwrap();
    for i in 0..3u64 {
        let anchor = now_ms - (i as i64) * 3_600_000;
        db.upsert_window_merge(
            &window_key(WindowKind::Session, i, 99, anchor),
            &snap(
                std::env::var("TOKI_E2E_PEAK").ok().and_then(|v| v.parse().ok()).unwrap_or(4200)
                    + i as u16,
                anchor,
                &format!("e2e_limit_{i}"),
            ),
        )
        .unwrap();
    }

    let mut client = toki::sync::client::SyncClient::connect(&addr, false, false)
        .expect("TCP connect to the containerized server");
    client
        .auth(&jwt, "e2e-device", "e2e00000-0000-4000-8000-00000000e2e0", "codex")
        .expect("auth handshake");

    // Print the outcome rather than asserting it: the driving script checks
    // both the accepted case and the throttled case, which are both correct
    // depending on how recently this user last uploaded.
    let sent = toki::sync::thread::windows_sync_step_for_test(&db, "codex", now_ms, (0, 0), &mut client);
    println!("OUTCOME={}", if sent { "Sent" } else { "NotSent" });
}
