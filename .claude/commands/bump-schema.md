Bump the DB schema version after a structural change.

## When to use

Run this command after modifying any of the following:
- `StoredEvent` struct fields (src/common/types.rs)
- `RollupValue` struct fields (src/common/types.rs)
- Keyspace layout in `Database::open` (src/db.rs)
- Dictionary encoding format (src/db.rs)
- Checkpoint format (src/checkpoint.rs)
- Any change that makes existing serialized data incompatible with new code

## Instructions

1. Read `src/db.rs` and find the current `SCHEMA_VERSION` constant
2. Increment it by 1
3. Commit with message: `chore: bump schema version to {NEW_VERSION}`

This causes all existing databases to auto-reset on next daemon start. Users don't need to run `toki daemon reset` manually.
