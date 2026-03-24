# Project Rules

## Schema Version

When modifying any of the following, you MUST increment `SCHEMA_VERSION` in `src/db.rs`:
- `StoredEvent` struct fields (`src/common/types.rs`)
- `RollupValue` struct fields (`src/common/types.rs`)
- Keyspace layout in `Database::open` (`src/db.rs`)
- Dictionary encoding format (`src/db.rs`)
- Checkpoint format (`src/checkpoint.rs`)

This triggers automatic DB reset on next daemon start. Users don't need to run `toki daemon reset` manually.

Do NOT bump when changes are additive only (e.g. new provider, new keyspace, new optional field with default). Only bump when existing serialized data becomes incompatible with new code.
