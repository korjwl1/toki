# Project Rules

## Schema Version

When modifying any of the following, you MUST increment `SCHEMA_VERSION` in `src/db.rs`:
- `StoredEvent` struct fields (`src/common/types.rs`)
- Existing event-DB key/value encoding or keyspace layout in `Database::open` (`src/db.rs`)
- Dictionary encoding format (`src/db.rs`)
- Checkpoint format (`src/checkpoint.rs`)

This triggers automatic reset of the rebuildable `<provider>.fjall` event DB on
next daemon start. Users do not need to run `toki daemon reset` manually.

Rate-limit windows live in `<provider>.windows.fjall` under their own
`WINDOWS_SCHEMA_VERSION`. Never use an event schema bump to wipe window data;
window observations may not be reconstructable. An incompatible window change
needs an explicit versioned decoder/migration and must preserve unknown data.

Do NOT bump for additive changes that leave existing event data readable. Bump
only when the current event serialization/key interpretation becomes incompatible.
