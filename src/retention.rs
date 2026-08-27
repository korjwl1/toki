use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::db::Database;

#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub event_retention_days: u32,
    /// Retention for rate-limit window rows. Independent of event retention:
    /// windows are ~2 rows/day and power long-horizon statistics, so they keep
    /// their own (much longer) horizon. 0 = disabled.
    pub window_retention_days: u32,
}

// Default: all zeros (disabled). Derive is sufficient.

pub struct RetentionStats {
    pub events_deleted: u64,
    pub index_deleted: u64,
    pub windows_deleted: u64,
    /// Dictionary keys garbage-collected this pass. Returned (not just counted)
    /// so the writer can evict them from its in-memory dict cache.
    pub dict_removed: Vec<String>,
    pub elapsed: Duration,
}

pub fn run_retention(
    db: &Database,
    policy: &RetentionPolicy,
) -> Result<RetentionStats, fjall::Error> {
    let t = Instant::now();

    // Window rows age out on their own horizon, before the event-retention
    // early return: event retention defaults to disabled, window retention
    // does not.
    // Close out rows orphaned open by a restart. Deliberately OUTSIDE the
    // retention toggle below: disabling retention must not leave restart
    // orphans finalized=false forever.
    // Non-panicking: these now run on EVERY pass in the default config (window
    // retention defaults to 730d while event retention defaults to off), and a
    // clock behind the epoch would otherwise kill the writer thread — after
    // which every DB write fails silently.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    {
        if let Err(e) = db.finalize_stale_windows(now_ms) {
            eprintln!("[toki] stale-window finalize error (continuing): {}", e);
        }
    }
    let windows_deleted = if policy.window_retention_days > 0 {
        let cutoff = now_ms - (policy.window_retention_days as i64) * 86_400_000;
        // A windows sweep failure must never suppress the pre-existing event
        // retention below (audit: `?` here would early-return the whole pass).
        match db.delete_windows_before(cutoff) {
            Ok(n) => n as u64,
            Err(e) => {
                eprintln!("[toki] windows retention error (continuing): {}", e);
                0
            }
        }
    } else {
        0
    };

    // 0 = disabled
    if policy.event_retention_days == 0 {
        return Ok(RetentionStats {
            events_deleted: 0,
            index_deleted: 0,
            windows_deleted,
            dict_removed: Vec::new(),
            elapsed: t.elapsed(),
        });
    }

    let cutoff = now_ms - (policy.event_retention_days as i64) * 86_400_000;

    let events_deleted = db.delete_events_before(cutoff)?;

    // Reclaim session/project index entries older than the cutoff. Run whenever
    // retention is enabled so an accumulated backlog (from older builds that only
    // deleted events) drains, not just when this pass removed events.
    let index_deleted = db.delete_index_before(cutoff)?;

    // Any deletion this pass may have orphaned dict entries — record that a GC is
    // owed durably, so it survives a crash and is retried even if the GC below
    // fails or a later pass sees no new deletions.
    if events_deleted > 0 || index_deleted > 0 {
        db.set_pending_dict_gc(true)?;
    }

    // Dictionary GC: drop entries no longer referenced by any surviving event.
    // The full event scan it needs is only worth running when there is work, so
    // it is gated on the persisted pending marker rather than this pass's
    // deletions alone. Also force it when the events keyspace is empty but the
    // dict still has rows: those are all orphans, and an already-empty DB would
    // never set the marker via a deletion. The marker is cleared ONLY after the
    // GC completes — if collect/gc errors out below, the `?` returns before the
    // clear, so the next pass retries.
    let orphans_in_empty_db = db.events_is_empty() && !db.dict_is_empty();
    let dict_removed = if db.pending_dict_gc()? || orphans_in_empty_db {
        let live = db.collect_live_dict_ids()?;
        let removed = db.gc_dict(&live)?;
        db.set_pending_dict_gc(false)?;
        removed
    } else {
        Vec::new()
    };

    Ok(RetentionStats {
        events_deleted,
        windows_deleted,
        index_deleted,
        dict_removed,
        elapsed: t.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::StoredEvent;

    #[test]
    fn test_retention_deletes_old_events() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let old_ts = now_ms - 100 * 86_400_000; // 100 days ago
        let recent_ts = now_ms - 10 * 86_400_000; // 10 days ago

        let event = StoredEvent {
            model_id: 1,
            session_id: 1,
            source_file_id: 1,
            project_name_id: 0,
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };

        db.insert_event(old_ts, "old", &event).unwrap();
        db.insert_event(recent_ts, "recent", &event).unwrap();

        let policy = RetentionPolicy {
            event_retention_days: 90,
            ..Default::default()
        };

        let stats = run_retention(&db, &policy).unwrap();
        assert_eq!(stats.events_deleted, 1);

        let remaining = db.query_events_range(0, now_ms + 1000).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1, "recent");
    }

    #[test]
    fn test_retention_cleans_indexes_and_dict() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let old_ts = now_ms - 100 * 86_400_000; // aged out
        let recent_ts = now_ms - 10 * 86_400_000; // kept

        // Dictionary: shared model, plus per-session and per-project entries.
        let mut batch = db.batch();
        db.dict_put(&mut batch, "model", 1);
        db.dict_put(&mut batch, "sess-old", 2);
        db.dict_put(&mut batch, "sess-recent", 3);
        db.dict_put(&mut batch, "proj-old", 4);
        db.dict_put(&mut batch, "proj-recent", 5);
        batch.commit().unwrap();

        let old_event = StoredEvent {
            model_id: 1,
            session_id: 2,
            source_file_id: 0,
            project_name_id: 4,
            input_tokens: 1,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let recent_event = StoredEvent {
            model_id: 1,
            session_id: 3,
            source_file_id: 0,
            project_name_id: 5,
            input_tokens: 1,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        db.insert_event(old_ts, "old", &old_event).unwrap();
        db.insert_event(recent_ts, "recent", &recent_event).unwrap();

        let mut idx = db.batch();
        db.insert_session_index(&mut idx, "sess-old", old_ts, "old");
        db.insert_session_index(&mut idx, "sess-recent", recent_ts, "recent");
        db.insert_project_index(&mut idx, "proj-old", old_ts, "old");
        db.insert_project_index(&mut idx, "proj-recent", recent_ts, "recent");
        idx.commit().unwrap();

        let policy = RetentionPolicy {
            event_retention_days: 90,
            ..Default::default()
        };
        let stats = run_retention(&db, &policy).unwrap();

        assert_eq!(stats.events_deleted, 1);
        // Old session + project index entries removed; recent ones survive.
        assert_eq!(db.list_sessions().unwrap(), vec!["sess-recent".to_string()]);
        assert_eq!(db.list_projects().unwrap(), vec!["proj-recent".to_string()]);
        assert_eq!(stats.index_deleted, 2);

        // Dict: entries referenced by the surviving event remain; unreferenced go.
        let dict = db.load_dict_reverse().unwrap();
        assert_eq!(dict.get(&1).map(|s| s.as_str()), Some("model")); // shared
        assert_eq!(dict.get(&3).map(|s| s.as_str()), Some("sess-recent")); // referenced
        assert_eq!(dict.get(&5).map(|s| s.as_str()), Some("proj-recent")); // referenced
        assert!(!dict.contains_key(&2), "sess-old dict entry must be GC'd");
        assert!(!dict.contains_key(&4), "proj-old dict entry must be GC'd");
        assert!(stats.dict_removed.contains(&"sess-old".to_string()));
        assert!(stats.dict_removed.contains(&"proj-old".to_string()));
    }

    #[test]
    fn test_retention_gcs_orphans_in_empty_events_db() {
        // No events at all, but the dict still holds entries — every one is an
        // orphan. A deletion-gated GC would never fire (nothing to delete), so the
        // empty-events force-path must clean them.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();

        let mut batch = db.batch();
        db.dict_put(&mut batch, "orphan-a", 1);
        db.dict_put(&mut batch, "orphan-b", 2);
        batch.commit().unwrap();
        assert!(db.events_is_empty());
        assert!(!db.dict_is_empty());

        let policy = RetentionPolicy {
            event_retention_days: 90,
            ..Default::default()
        };
        let stats = run_retention(&db, &policy).unwrap();

        assert_eq!(stats.events_deleted, 0);
        assert!(
            db.load_dict_reverse().unwrap().is_empty(),
            "orphan dict entries must be GC'd in an empty events DB"
        );
        assert!(stats.dict_removed.contains(&"orphan-a".to_string()));
        assert!(stats.dict_removed.contains(&"orphan-b".to_string()));
    }

    #[test]
    fn test_retention_pending_marker_retries_gc_without_new_deletions() {
        // Simulate a prior pass that deleted rows (marker set) but whose dict GC
        // never completed. The next pass must run GC from the marker alone — even
        // though it deletes nothing new — and clear the marker on success.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();

        let mut batch = db.batch();
        db.dict_put(&mut batch, "model", 1);
        db.dict_put(&mut batch, "orphan-2", 2);
        db.dict_put(&mut batch, "orphan-3", 3);
        batch.commit().unwrap();

        // A recent event keeps id 1 (and the sentinel 0) live; nothing ages out.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let recent_ts = now_ms - 86_400_000;
        let ev = StoredEvent {
            model_id: 1,
            session_id: 1,
            source_file_id: 0,
            project_name_id: 0,
            input_tokens: 1,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        db.insert_event(recent_ts, "m1", &ev).unwrap();

        db.set_pending_dict_gc(true).unwrap();

        let policy = RetentionPolicy {
            event_retention_days: 90,
            ..Default::default()
        };
        let stats = run_retention(&db, &policy).unwrap();

        assert_eq!(stats.events_deleted, 0, "recent event must survive");
        assert!(
            stats.dict_removed.contains(&"orphan-2".to_string()),
            "pending marker must force the owed GC"
        );
        assert!(stats.dict_removed.contains(&"orphan-3".to_string()));
        let dict = db.load_dict_reverse().unwrap();
        assert_eq!(
            dict.get(&1).map(|s| s.as_str()),
            Some("model"),
            "live entry must survive"
        );
        assert!(!dict.contains_key(&2));
        assert!(
            !db.pending_dict_gc().unwrap(),
            "marker must be cleared after a successful GC"
        );
    }

    #[test]
    fn test_retention_disabled_noop() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();
        let policy = RetentionPolicy {
            event_retention_days: 0,
            ..Default::default()
        };
        let stats = run_retention(&db, &policy).unwrap();
        assert_eq!(stats.events_deleted, 0);
        assert_eq!(stats.index_deleted, 0);
        assert!(stats.dict_removed.is_empty());
    }
}
