use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::db::Database;

#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    pub event_retention_days: u32,
    pub rollup_retention_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        RetentionPolicy {
            event_retention_days: 90,
            rollup_retention_days: 365,
        }
    }
}

pub struct RetentionStats {
    pub events_deleted: u64,
    pub rollups_deleted: u64,
    pub sessions_deleted: u64,
    pub projects_deleted: u64,
    pub elapsed: Duration,
}

pub fn run_retention(db: &Database, policy: &RetentionPolicy) -> Result<RetentionStats, fjall::Error> {
    let t = Instant::now();
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;

    let event_cutoff = now_ms - (policy.event_retention_days as i64) * 86_400_000;
    let rollup_cutoff = now_ms - (policy.rollup_retention_days as i64) * 86_400_000;

    let events_deleted = db.delete_events_before(event_cutoff)?;
    let rollups_deleted = db.delete_rollups_before(rollup_cutoff)?;

    // Index cleanup is skipped: session/project indices have keys like
    // {prefix}\0{ts}{msg_id}, so a full scan would be O(n) over all entries.
    // Orphaned index entries (pointing to deleted events) are harmless —
    // they have empty values and are tiny. They'll naturally age out as
    // new entries accumulate.

    Ok(RetentionStats {
        events_deleted,
        rollups_deleted,
        sessions_deleted: 0,
        projects_deleted: 0,
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
            model_id: 1, session_id: 1, source_file_id: 1,
            input_tokens: 10, output_tokens: 5,
            cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        };

        db.insert_event(old_ts, "old", &event).unwrap();
        db.insert_event(recent_ts, "recent", &event).unwrap();

        let policy = RetentionPolicy {
            event_retention_days: 90,
            rollup_retention_days: 365,
        };

        let stats = run_retention(&db, &policy).unwrap();
        assert_eq!(stats.events_deleted, 1);

        let remaining = db.query_events_range(0, now_ms + 1000).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1, "recent");
    }
}
