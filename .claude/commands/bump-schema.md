Bump the DB schema version after a structural change.

## When to use

Run this command after modifying any of the following:
- `StoredEvent` struct fields (src/common/types.rs)
- Existing event-DB key/value encoding or keyspace layout in `Database::open` (src/db.rs)
- Dictionary encoding format (src/db.rs)
- Checkpoint format (src/checkpoint.rs)
- Any change that makes existing serialized data incompatible with new code

## Instructions

1. Read `src/db.rs` and find the current `SCHEMA_VERSION` constant
2. Increment it by 1
3. Commit with message: `chore: bump schema version to {NEW_VERSION}`

This resets only the rebuildable `<provider>.fjall` event databases on next
daemon start. It must not reset `<provider>.windows.fjall`: window history has
its own `WINDOWS_SCHEMA_VERSION` and may not be reconstructable.
