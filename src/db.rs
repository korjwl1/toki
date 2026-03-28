use std::collections::HashMap;
use std::path::Path;

use fjall::{Database as FjallDatabase, Keyspace, KeyspaceCreateOptions};

use crate::common::types::{FileCheckpoint, RollupValue, StoredEvent};

pub struct Database {
    db: FjallDatabase,
    checkpoints: Keyspace,
    meta: Keyspace,
    events: Keyspace,
    rollups: Keyspace,
    idx_sessions: Keyspace,
    idx_projects: Keyspace,
    dict: Keyspace,
}

/// Schema version. Increment when StoredEvent, RollupValue, or keyspace layout changes.
/// Mismatched version triggers automatic DB reset + cold start rebuild.
pub const SCHEMA_VERSION: u32 = 2;

impl Database {
    pub fn open(path: &Path) -> Result<Self, fjall::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        // Check schema version; wipe and recreate if mismatched
        if path.exists() {
            if let Ok(db) = FjallDatabase::builder(path).open() {
                let opts = || KeyspaceCreateOptions::default();
                if let Ok(meta) = db.keyspace("meta", opts) {
                    let stored_version = meta.get("schema_version")
                        .ok()
                        .flatten()
                        .and_then(|b| String::from_utf8_lossy(&b).parse::<u32>().ok())
                        .unwrap_or(0);

                    if stored_version != SCHEMA_VERSION {
                        eprintln!("[toki] Schema version changed ({} → {}), resetting database...",
                            stored_version, SCHEMA_VERSION);
                        drop(meta);
                        drop(db);
                        std::fs::remove_dir_all(path).ok();
                    }
                }
            }
        }

        let db = FjallDatabase::builder(path)
            .open()?;

        let opts = || KeyspaceCreateOptions::default();
        let checkpoints = db.keyspace("checkpoints", opts)?;
        let meta = db.keyspace("meta", opts)?;
        let events = db.keyspace("events", opts)?;
        let rollups = db.keyspace("rollups", opts)?;
        let idx_sessions = db.keyspace("idx_sessions", opts)?;
        let idx_projects = db.keyspace("idx_projects", opts)?;
        let dict = db.keyspace("dict", opts)?;

        // Write current schema version
        meta.insert("schema_version", SCHEMA_VERSION.to_string().as_bytes())?;

        Ok(Database { db, checkpoints, meta, events, rollups, idx_sessions, idx_projects, dict })
    }

    pub fn inner(&self) -> &FjallDatabase {
        &self.db
    }

    // -- Checkpoint operations --

    pub fn load_all_checkpoints(&self) -> Result<Vec<FileCheckpoint>, fjall::Error> {
        let mut checkpoints = Vec::new();

        for guard in self.checkpoints.iter() {
            let kv = guard.into_inner()?;
            if let Ok(cp) = bincode::deserialize::<FileCheckpoint>(&kv.1) {
                checkpoints.push(cp);
            }
        }

        Ok(checkpoints)
    }

    pub fn upsert_checkpoint(&self, cp: &FileCheckpoint) -> Result<(), fjall::Error> {
        let bytes = bincode::serialize(cp).expect("FileCheckpoint serialization failed");
        self.checkpoints.insert(cp.file_path.as_str(), bytes)?;
        Ok(())
    }

    pub fn flush_checkpoints(&self, checkpoints: &[FileCheckpoint]) -> Result<(), fjall::Error> {
        if checkpoints.is_empty() {
            return Ok(());
        }
        let mut batch = self.db.batch();
        for cp in checkpoints {
            let bytes = bincode::serialize(cp).expect("FileCheckpoint serialization failed");
            batch.insert(&self.checkpoints, cp.file_path.as_str(), bytes);
        }
        batch.commit()?;
        Ok(())
    }

    pub fn get_checkpoint(&self, file_path: &str) -> Result<Option<FileCheckpoint>, fjall::Error> {
        match self.checkpoints.get(file_path)? {
            Some(bytes) => {
                let cp = bincode::deserialize::<FileCheckpoint>(&bytes)
                    .expect("FileCheckpoint deserialization failed");
                Ok(Some(cp))
            }
            None => Ok(None),
        }
    }

    pub fn remove_checkpoint(&self, file_path: &str) -> Result<(), fjall::Error> {
        self.checkpoints.remove(file_path)?;
        Ok(())
    }

    /// Remove all checkpoints (used for forced cold start).
    pub fn clear_checkpoints(&self) -> Result<(), fjall::Error> {
        self.checkpoints.clear()?;
        Ok(())
    }

    // -- Settings operations --

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, fjall::Error> {
        match self.meta.get(key)? {
            Some(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).to_string())),
            None => Ok(None),
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), fjall::Error> {
        self.meta.insert(key, value)?;
        Ok(())
    }

    // -- Event operations --

    /// Build event key: [ts_ms big-endian 8 bytes][message_id bytes]
    fn event_key(ts_ms: i64, message_id: &str) -> Vec<u8> {
        let mut key = Vec::with_capacity(8 + message_id.len());
        key.extend_from_slice(&ts_ms.to_be_bytes());
        key.extend_from_slice(message_id.as_bytes());
        key
    }

    pub fn insert_event(&self, ts_ms: i64, message_id: &str, event: &StoredEvent) -> Result<(), fjall::Error> {
        let key = Self::event_key(ts_ms, message_id);
        let value = bincode::serialize(event).expect("StoredEvent serialization failed");
        self.events.insert(key, value)?;
        Ok(())
    }

    /// Insert a batch of events in a single transaction.
    pub fn insert_event_batch(&self, batch: &mut fjall::OwnedWriteBatch, ts_ms: i64, message_id: &str, event: &StoredEvent) {
        let key = Self::event_key(ts_ms, message_id);
        let value = bincode::serialize(event).expect("StoredEvent serialization failed");
        batch.insert(&self.events, key, value);
    }

    // -- Rollup operations --

    /// Build rollup key: [hour_ts big-endian 8 bytes][model_name bytes]
    fn rollup_key(hour_ts: i64, model_name: &str) -> Vec<u8> {
        let mut key = Vec::with_capacity(8 + model_name.len());
        key.extend_from_slice(&hour_ts.to_be_bytes());
        key.extend_from_slice(model_name.as_bytes());
        key
    }

    pub fn get_rollup(&self, hour_ts: i64, model_name: &str) -> Result<Option<RollupValue>, fjall::Error> {
        let key = Self::rollup_key(hour_ts, model_name);
        match self.rollups.get(&key)? {
            Some(bytes) => Ok(bincode::deserialize::<RollupValue>(&bytes).ok()),
            None => Ok(None),
        }
    }

    pub fn upsert_rollup(&self, batch: &mut fjall::OwnedWriteBatch, hour_ts: i64, model_name: &str, rollup: &RollupValue) {
        let key = Self::rollup_key(hour_ts, model_name);
        let value = bincode::serialize(rollup).expect("RollupValue serialization failed");
        batch.insert(&self.rollups, key, value);
    }

    // -- Index operations --

    /// Insert session index entry: key = "{session_id}\0[ts:8][msg_id]", value = empty
    pub fn insert_session_index(&self, batch: &mut fjall::OwnedWriteBatch, session_id: &str, ts_ms: i64, message_id: &str) {
        let mut key = Vec::with_capacity(session_id.len() + 1 + 8 + message_id.len());
        key.extend_from_slice(session_id.as_bytes());
        key.push(0);
        key.extend_from_slice(&ts_ms.to_be_bytes());
        key.extend_from_slice(message_id.as_bytes());
        batch.insert(&self.idx_sessions, key, b"");
    }

    /// Insert project index entry: key = "{project}\0[ts:8][msg_id]", value = empty
    pub fn insert_project_index(&self, batch: &mut fjall::OwnedWriteBatch, project: &str, ts_ms: i64, message_id: &str) {
        let mut key = Vec::with_capacity(project.len() + 1 + 8 + message_id.len());
        key.extend_from_slice(project.as_bytes());
        key.push(0);
        key.extend_from_slice(&ts_ms.to_be_bytes());
        key.extend_from_slice(message_id.as_bytes());
        batch.insert(&self.idx_projects, key, b"");
    }

    /// List distinct session IDs from the session index.
    /// Keys are sorted in LSM-tree, so consecutive duplicates are adjacent.
    pub fn list_sessions(&self) -> Result<Vec<String>, fjall::Error> {
        let mut sessions = Vec::new();
        let mut last_prefix: Vec<u8> = Vec::new();
        for guard in self.idx_sessions.iter() {
            let kv = guard.into_inner()?;
            let key = &kv.0;
            let null_pos = key.iter().position(|&b| b == 0).unwrap_or(key.len());
            let prefix = &key[..null_pos];
            if prefix != last_prefix.as_slice() {
                last_prefix.clear();
                last_prefix.extend_from_slice(prefix);
                sessions.push(String::from_utf8_lossy(prefix).into_owned());
            }
        }
        Ok(sessions)
    }

    /// List distinct project names from the project index.
    pub fn list_projects(&self) -> Result<Vec<String>, fjall::Error> {
        let mut projects = Vec::new();
        let mut last_prefix: Vec<u8> = Vec::new();
        for guard in self.idx_projects.iter() {
            let kv = guard.into_inner()?;
            let key = &kv.0;
            let null_pos = key.iter().position(|&b| b == 0).unwrap_or(key.len());
            let prefix = &key[..null_pos];
            if prefix != last_prefix.as_slice() {
                last_prefix.clear();
                last_prefix.extend_from_slice(prefix);
                projects.push(String::from_utf8_lossy(prefix).into_owned());
            }
        }
        Ok(projects)
    }

    // -- Dictionary operations --

    pub fn dict_get(&self, key: &str) -> Result<Option<u32>, fjall::Error> {
        match self.dict.get(key)? {
            Some(bytes) => Ok(bincode::deserialize::<u32>(&bytes).ok()),
            None => Ok(None),
        }
    }

    pub fn dict_put(&self, batch: &mut fjall::OwnedWriteBatch, key: &str, id: u32) {
        let value = bincode::serialize(&id).expect("dict id serialization failed");
        batch.insert(&self.dict, key, value);
    }

    /// Load the full dictionary for reverse lookup (id → string).
    pub fn load_dict_reverse(&self) -> Result<HashMap<u32, String>, fjall::Error> {
        let mut map = HashMap::new();
        for guard in self.dict.iter() {
            let kv = guard.into_inner()?;
            let key = String::from_utf8_lossy(&kv.0).into_owned();
            if let Ok(id) = bincode::deserialize::<u32>(&kv.1) {
                map.insert(id, key);
            }
        }
        Ok(map)
    }

    /// Load the full dictionary for forward lookup (string → id).
    pub fn load_dict_forward(&self) -> Result<HashMap<String, u32>, fjall::Error> {
        let mut map = HashMap::new();
        for guard in self.dict.iter() {
            let kv = guard.into_inner()?;
            let key = String::from_utf8_lossy(&kv.0).into_owned();
            if let Ok(id) = bincode::deserialize::<u32>(&kv.1) {
                map.insert(key, id);
            }
        }
        Ok(map)
    }

    // -- Query operations --

    /// Query events in a timestamp range [since_ms, until_ms], returning at most `limit` results.
    pub fn query_events_range_limit(&self, since_ms: i64, until_ms: i64, limit: usize) -> Result<Vec<(i64, String, StoredEvent)>, fjall::Error> {
        let start_key = since_ms.to_be_bytes().to_vec();

        let mut results = Vec::new();
        for guard in self.events.range(start_key..).take(limit) {
            let kv = guard.into_inner()?;
            let key = &kv.0;
            if key.len() < 8 { continue; }
            let ts_bytes: [u8; 8] = match key[..8].try_into() {
                Ok(b) => b,
                Err(_) => continue,
            };
            let ts = i64::from_be_bytes(ts_bytes);
            if ts > until_ms { break; }
            let msg_id = String::from_utf8_lossy(&key[8..]).into_owned();
            if let Ok(event) = bincode::deserialize::<StoredEvent>(&kv.1) {
                results.push((ts, msg_id, event));
            }
        }
        Ok(results)
    }

    /// Query events in a timestamp range [since_ms, until_ms].
    pub fn query_events_range(&self, since_ms: i64, until_ms: i64) -> Result<Vec<(i64, String, StoredEvent)>, fjall::Error> {
        let start_key = since_ms.to_be_bytes().to_vec();

        let mut results = Vec::new();
        for guard in self.events.range(start_key..) {
            let kv = guard.into_inner()?;
            let key = &kv.0;
            if key.len() < 8 { continue; }
            let ts_bytes: [u8; 8] = match key[..8].try_into() {
                Ok(b) => b,
                Err(_) => continue,
            };
            let ts = i64::from_be_bytes(ts_bytes);
            if ts > until_ms { break; }
            let msg_id = String::from_utf8_lossy(&key[8..]).into_owned();
            if let Ok(event) = bincode::deserialize::<StoredEvent>(&kv.1) {
                results.push((ts, msg_id, event));
            }
        }
        Ok(results)
    }

    /// Check if any rollup data exists (O(1) — reads only first entry).
    pub fn has_any_rollups(&self) -> bool {
        self.rollups.first_key_value().is_some()
    }

    /// Get the actual data time range from rollups (O(1) each — B-tree first/last key).
    /// Returns (earliest_ms, latest_ms) or None if no data.
    pub fn data_range(&self) -> Option<(i64, i64)> {
        let extract_ts = |guard: fjall::Guard| -> Option<i64> {
            let kv = guard.into_inner().ok()?;
            let key = &kv.0;
            if key.len() < 8 { return None; }
            Some(i64::from_be_bytes(key[..8].try_into().ok()?))
        };

        let first_ts = extract_ts(self.rollups.first_key_value()?)?;
        let last_ts = extract_ts(self.rollups.last_key_value()?)?;
        Some((first_ts, last_ts))
    }

    /// Iterate rollups in [since_ms, until_ms] range, calling `f` for each.
    /// Avoids allocating a Vec when only aggregation is needed.
    pub fn for_each_rollup<F>(&self, since_ms: i64, until_ms: i64, mut f: F) -> Result<(), fjall::Error>
    where
        F: FnMut(i64, String, RollupValue),
    {
        let start_key = since_ms.to_be_bytes().to_vec();
        for guard in self.rollups.range(start_key..) {
            let kv = guard.into_inner()?;
            let key = &kv.0;
            if key.len() < 8 { continue; }
            let ts_bytes: [u8; 8] = match key[..8].try_into() {
                Ok(b) => b,
                Err(_) => continue,
            };
            let ts = i64::from_be_bytes(ts_bytes);
            if ts > until_ms { break; }
            let model = String::from_utf8_lossy(&key[8..]).into_owned();
            if let Ok(rollup) = bincode::deserialize::<RollupValue>(&kv.1) {
                f(ts, model, rollup);
            }
        }
        Ok(())
    }

    /// Iterate events in [since_ms, until_ms] range, calling `f` for each.
    pub fn for_each_event<F>(&self, since_ms: i64, until_ms: i64, mut f: F) -> Result<(), fjall::Error>
    where
        F: FnMut(i64, StoredEvent),
    {
        let start_key = since_ms.to_be_bytes().to_vec();
        for guard in self.events.range(start_key..) {
            let kv = guard.into_inner()?;
            let key = &kv.0;
            if key.len() < 8 { continue; }
            let ts_bytes: [u8; 8] = match key[..8].try_into() {
                Ok(b) => b,
                Err(_) => continue,
            };
            let ts = i64::from_be_bytes(ts_bytes);
            if ts > until_ms { break; }
            if let Ok(event) = bincode::deserialize::<StoredEvent>(&kv.1) {
                f(ts, event);
            }
        }
        Ok(())
    }

    // -- Deletion operations --

    /// Delete events with timestamp before cutoff_ms. Returns count deleted.
    pub fn delete_events_before(&self, cutoff_ms: i64) -> Result<u64, fjall::Error> {
        let cutoff_key = cutoff_ms.to_be_bytes().to_vec();
        let mut deleted = 0u64;
        let mut keys_to_delete = Vec::new();

        for guard in self.events.iter() {
            let kv = guard.into_inner()?;
            if kv.0.as_ref() >= cutoff_key.as_slice() {
                break;
            }
            keys_to_delete.push(kv.0.to_vec());
            if keys_to_delete.len() >= 1000 {
                let mut batch = self.db.batch();
                for key in keys_to_delete.drain(..) {
                    batch.remove(&self.events, key);
                    deleted += 1;
                }
                batch.commit()?;
            }
        }
        if !keys_to_delete.is_empty() {
            let mut batch = self.db.batch();
            for key in keys_to_delete {
                batch.remove(&self.events, key);
                deleted += 1;
            }
            batch.commit()?;
        }
        Ok(deleted)
    }

    /// Delete rollups with timestamp before cutoff_ms. Returns count deleted.
    pub fn delete_rollups_before(&self, cutoff_ms: i64) -> Result<u64, fjall::Error> {
        let cutoff_key = cutoff_ms.to_be_bytes().to_vec();
        let mut deleted = 0u64;
        let mut keys_to_delete = Vec::new();

        for guard in self.rollups.iter() {
            let kv = guard.into_inner()?;
            if kv.0.as_ref() >= cutoff_key.as_slice() {
                break;
            }
            keys_to_delete.push(kv.0.to_vec());
            if keys_to_delete.len() >= 1000 {
                let mut batch = self.db.batch();
                for key in keys_to_delete.drain(..) {
                    batch.remove(&self.rollups, key);
                    deleted += 1;
                }
                batch.commit()?;
            }
        }
        if !keys_to_delete.is_empty() {
            let mut batch = self.db.batch();
            for key in keys_to_delete {
                batch.remove(&self.rollups, key);
                deleted += 1;
            }
            batch.commit()?;
        }
        Ok(deleted)
    }

    /// Create a new batch.
    pub fn batch(&self) -> fjall::OwnedWriteBatch {
        self.db.batch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::RollupValue;

    fn temp_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.fjall");
        let db = Database::open(&db_path).unwrap();
        (db, dir)
    }

    #[test]
    fn test_checkpoint_round_trip() {
        let (db, _dir) = temp_db();

        let cp = FileCheckpoint {
            file_path: "/test/file.jsonl".to_string(),
            last_line_len: 256,
            last_line_hash: 12345678901234,
        };

        db.upsert_checkpoint(&cp).unwrap();

        let loaded = db.get_checkpoint("/test/file.jsonl").unwrap().unwrap();
        assert_eq!(loaded.file_path, cp.file_path);
        assert_eq!(loaded.last_line_len, cp.last_line_len);
        assert_eq!(loaded.last_line_hash, cp.last_line_hash);
    }

    #[test]
    fn test_checkpoint_upsert_overwrites() {
        let (db, _dir) = temp_db();

        let cp1 = FileCheckpoint {
            file_path: "/test/file.jsonl".to_string(),
            last_line_len: 100,
            last_line_hash: 111,
        };
        db.upsert_checkpoint(&cp1).unwrap();

        let cp2 = FileCheckpoint {
            file_path: "/test/file.jsonl".to_string(),
            last_line_len: 200,
            last_line_hash: 222,
        };
        db.upsert_checkpoint(&cp2).unwrap();

        let loaded = db.get_checkpoint("/test/file.jsonl").unwrap().unwrap();
        assert_eq!(loaded.last_line_len, 200);
        assert_eq!(loaded.last_line_hash, 222);
    }

    #[test]
    fn test_load_all_checkpoints() {
        let (db, _dir) = temp_db();

        for i in 0..3u64 {
            let cp = FileCheckpoint {
                file_path: format!("/test/file{}.jsonl", i),
                last_line_len: i * 100,
                last_line_hash: i * 1000,
            };
            db.upsert_checkpoint(&cp).unwrap();
        }

        let all = db.load_all_checkpoints().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_flush_checkpoints_batch() {
        let (db, _dir) = temp_db();

        let cps: Vec<FileCheckpoint> = (0..5u64)
            .map(|i| FileCheckpoint {
                file_path: format!("/batch/file{}.jsonl", i),
                last_line_len: i * 50,
                last_line_hash: i * 500,
            })
            .collect();

        db.flush_checkpoints(&cps).unwrap();

        let all = db.load_all_checkpoints().unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn test_remove_checkpoint() {
        let (db, _dir) = temp_db();

        let cp = FileCheckpoint {
            file_path: "/remove/me.jsonl".to_string(),
            last_line_len: 42,
            last_line_hash: 999,
        };
        db.upsert_checkpoint(&cp).unwrap();

        db.remove_checkpoint("/remove/me.jsonl").unwrap();
        assert!(db.get_checkpoint("/remove/me.jsonl").unwrap().is_none());
    }

    #[test]
    fn test_settings_round_trip() {
        let (db, _dir) = temp_db();

        db.set_setting("claude_code_root", "/custom/path").unwrap();

        let val = db.get_setting("claude_code_root").unwrap().unwrap();
        assert_eq!(val, "/custom/path");
    }

    #[test]
    fn test_settings_missing_key() {
        let (db, _dir) = temp_db();

        assert!(db.get_setting("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_settings_overwrite() {
        let (db, _dir) = temp_db();

        db.set_setting("key", "value1").unwrap();
        db.set_setting("key", "value2").unwrap();

        let val = db.get_setting("key").unwrap().unwrap();
        assert_eq!(val, "value2");
    }

    #[test]
    fn test_event_insert_and_query() {
        let (db, _dir) = temp_db();

        let event = StoredEvent {
            model_id: 1,
            session_id: 2,
            source_file_id: 3,
            project_name_id: 0,
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 20,
        };

        db.insert_event(1000, "msg1", &event).unwrap();
        db.insert_event(2000, "msg2", &event).unwrap();
        db.insert_event(3000, "msg3", &event).unwrap();

        let results = db.query_events_range(1000, 2000).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 1000);
        assert_eq!(results[1].0, 2000);
    }

    #[test]
    fn test_rollup_upsert_and_query() {
        let (db, _dir) = temp_db();

        let hour_ts = 3_600_000i64;
        let rollup = RollupValue {
            input: 100,
            output: 50,
            cache_create: 10,
            cache_read: 20,
            count: 5,
        };

        let mut batch = db.batch();
        db.upsert_rollup(&mut batch, hour_ts, "claude-opus-4-6", &rollup);
        batch.commit().unwrap();

        let loaded = db.get_rollup(hour_ts, "claude-opus-4-6").unwrap().unwrap();
        assert_eq!(loaded.input, 100);
        assert_eq!(loaded.count, 5);
    }

    #[test]
    fn test_dict_round_trip() {
        let (db, _dir) = temp_db();

        let mut batch = db.batch();
        db.dict_put(&mut batch, "claude-opus-4-6", 1);
        db.dict_put(&mut batch, "session-123", 2);
        batch.commit().unwrap();

        assert_eq!(db.dict_get("claude-opus-4-6").unwrap(), Some(1));
        assert_eq!(db.dict_get("session-123").unwrap(), Some(2));
        assert_eq!(db.dict_get("nonexistent").unwrap(), None);

        let reverse = db.load_dict_reverse().unwrap();
        assert_eq!(reverse.get(&1).unwrap(), "claude-opus-4-6");
        assert_eq!(reverse.get(&2).unwrap(), "session-123");
    }

    #[test]
    fn test_delete_events_before() {
        let (db, _dir) = temp_db();

        let event = StoredEvent {
            model_id: 1, session_id: 1, source_file_id: 1, project_name_id: 0,
            input_tokens: 10, output_tokens: 5,
            cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        };
        db.insert_event(1000, "a", &event).unwrap();
        db.insert_event(2000, "b", &event).unwrap();
        db.insert_event(3000, "c", &event).unwrap();

        let deleted = db.delete_events_before(2000).unwrap();
        assert_eq!(deleted, 1); // only ts=1000

        let remaining = db.query_events_range(0, 10000).unwrap();
        assert_eq!(remaining.len(), 2);
    }
}
