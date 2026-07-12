use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::db::Database;

#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub event_retention_days: u32,
}

// Default: all zeros (disabled). Derive is sufficient.

pub struct RetentionStats {
    pub events_deleted: u64,
    pub index_deleted: u64,
    /// Dictionary keys garbage-collected this pass. Returned (not just counted)
    /// so the writer can evict them from its in-memory dict cache.
    pub dict_removed: Vec<String>,
    pub elapsed: Duration,
}

pub fn run_retention(db: &Database, policy: &RetentionPolicy) -> Result<RetentionStats, fjall::Error> {
    let t = Instant::now();

    // 0 = disabled
    if policy.event_retention_days == 0 {
        return Ok(RetentionStats {
            events_deleted: 0,
            index_deleted: 0,
            dict_removed: Vec::new(),
            elapsed: t.elapsed(),
        });
    }

    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let cutoff = now_ms - (policy.event_retention_days as i64) * 86_400_000;

    let events_deleted = db.delete_events_before(cutoff)?;

    // Reclaim session/project index entries older than the cutoff. Run whenever
    // retention is enabled so an accumulated backlog (from older builds that only
    // deleted events) drains, not just when this pass removed events.
    let index_deleted = db.delete_index_before(cutoff)?;

    // Dictionary GC: drop entries no longer referenced by any surviving event.
    // Gated on something actually having been removed so the full event scan it
    // requires only runs when there is work to do, keeping steady-state passes cheap.
    let dict_removed = if events_deleted > 0 || index_deleted > 0 {
        let live = db.collect_live_dict_ids()?;
        db.gc_dict(&live)?
    } else {
        Vec::new()
    };

    Ok(RetentionStats {
        events_deleted,
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

        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        let old_ts = now_ms - 100 * 86_400_000; // 100 days ago
        let recent_ts = now_ms - 10 * 86_400_000; // 10 days ago

        let event = StoredEvent {
            model_id: 1, session_id: 1, source_file_id: 1, project_name_id: 0,
            input_tokens: 10, output_tokens: 5,
            cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        };

        db.insert_event(old_ts, "old", &event).unwrap();
        db.insert_event(recent_ts, "recent", &event).unwrap();

        let policy = RetentionPolicy {
            event_retention_days: 90,
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

        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
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
            model_id: 1, session_id: 2, source_file_id: 0, project_name_id: 4,
            input_tokens: 1, output_tokens: 0,
            cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        };
        let recent_event = StoredEvent {
            model_id: 1, session_id: 3, source_file_id: 0, project_name_id: 5,
            input_tokens: 1, output_tokens: 0,
            cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        };
        db.insert_event(old_ts, "old", &old_event).unwrap();
        db.insert_event(recent_ts, "recent", &recent_event).unwrap();

        let mut idx = db.batch();
        db.insert_session_index(&mut idx, "sess-old", old_ts, "old");
        db.insert_session_index(&mut idx, "sess-recent", recent_ts, "recent");
        db.insert_project_index(&mut idx, "proj-old", old_ts, "old");
        db.insert_project_index(&mut idx, "proj-recent", recent_ts, "recent");
        idx.commit().unwrap();

        let policy = RetentionPolicy { event_retention_days: 90 };
        let stats = run_retention(&db, &policy).unwrap();

        assert_eq!(stats.events_deleted, 1);
        // Old session + project index entries removed; recent ones survive.
        assert_eq!(db.list_sessions().unwrap(), vec!["sess-recent".to_string()]);
        assert_eq!(db.list_projects().unwrap(), vec!["proj-recent".to_string()]);
        assert_eq!(stats.index_deleted, 2);

        // Dict: entries referenced by the surviving event remain; unreferenced go.
        let dict = db.load_dict_reverse().unwrap();
        assert_eq!(dict.get(&1).map(|s| s.as_str()), Some("model"));       // shared
        assert_eq!(dict.get(&3).map(|s| s.as_str()), Some("sess-recent")); // referenced
        assert_eq!(dict.get(&5).map(|s| s.as_str()), Some("proj-recent")); // referenced
        assert!(dict.get(&2).is_none(), "sess-old dict entry must be GC'd");
        assert!(dict.get(&4).is_none(), "proj-old dict entry must be GC'd");
        assert!(stats.dict_removed.contains(&"sess-old".to_string()));
        assert!(stats.dict_removed.contains(&"proj-old".to_string()));
    }

    #[test]
    fn test_retention_disabled_noop() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();
        let policy = RetentionPolicy { event_retention_days: 0 };
        let stats = run_retention(&db, &policy).unwrap();
        assert_eq!(stats.events_deleted, 0);
        assert_eq!(stats.index_deleted, 0);
        assert!(stats.dict_removed.is_empty());
    }
}
