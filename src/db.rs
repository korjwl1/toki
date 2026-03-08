use std::path::Path;

use redb::{Database as RedbDatabase, ReadableTable, TableDefinition};

use crate::common::types::FileCheckpoint;

const CHECKPOINTS: TableDefinition<&str, &[u8]> = TableDefinition::new("checkpoints");
const SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("settings");

pub struct Database {
    db: RedbDatabase,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let db = RedbDatabase::create(path)?;

        // Ensure tables exist
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(CHECKPOINTS)?;
            let _ = txn.open_table(SETTINGS)?;
        }
        txn.commit()?;

        Ok(Database { db })
    }

    // -- Checkpoint operations --

    pub fn load_all_checkpoints(&self) -> Result<Vec<FileCheckpoint>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHECKPOINTS)?;
        let mut checkpoints = Vec::new();

        let iter = table.iter()?;
        for entry in iter {
            let entry = entry?;
            let bytes = entry.1.value();
            if let Ok(cp) = bincode::deserialize::<FileCheckpoint>(bytes) {
                checkpoints.push(cp);
            }
        }

        Ok(checkpoints)
    }

    pub fn upsert_checkpoint(&self, cp: &FileCheckpoint) -> Result<(), redb::Error> {
        let bytes = bincode::serialize(cp).expect("FileCheckpoint serialization failed");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CHECKPOINTS)?;
            table.insert(cp.file_path.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn flush_checkpoints(&self, checkpoints: &[FileCheckpoint]) -> Result<(), redb::Error> {
        if checkpoints.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CHECKPOINTS)?;
            for cp in checkpoints {
                let bytes = bincode::serialize(cp).expect("FileCheckpoint serialization failed");
                table.insert(cp.file_path.as_str(), bytes.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_checkpoint(&self, file_path: &str) -> Result<Option<FileCheckpoint>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHECKPOINTS)?;
        match table.get(file_path)? {
            Some(bytes) => {
                let cp = bincode::deserialize::<FileCheckpoint>(bytes.value())
                    .expect("FileCheckpoint deserialization failed");
                Ok(Some(cp))
            }
            None => Ok(None),
        }
    }

    pub fn remove_checkpoint(&self, file_path: &str) -> Result<(), redb::Error> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CHECKPOINTS)?;
            table.remove(file_path)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Remove all checkpoints (used for forced cold start).
    pub fn clear_checkpoints(&self) -> Result<(), redb::Error> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CHECKPOINTS)?;
            // Collect keys first, then remove
            let keys: Vec<String> = {
                let iter = table.iter()?;
                iter.filter_map(|e| e.ok().map(|e| e.0.value().to_string())).collect()
            };
            for key in &keys {
                table.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    // -- Settings operations --

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SETTINGS)?;
        match table.get(key)? {
            Some(val) => Ok(Some(val.value().to_string())),
            None => Ok(None),
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), redb::Error> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(SETTINGS)?;
            table.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
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
}
