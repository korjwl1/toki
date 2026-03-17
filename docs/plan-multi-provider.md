# Multi-Provider Support Implementation Plan

## Status

**Draft** -- 2026-03-17

## Overview

toki currently only supports Claude Code. This plan adds a provider abstraction layer and a second provider (Codex CLI), with an architecture designed to accommodate future providers (Gemini CLI, etc.).

### Key Design Decisions

1. **Per-provider token fields** -- Each provider keeps its own token field definitions, DB schema types (`StoredEvent`, `RollupValue`, `TokenFields`), and display logic. Cross-provider aggregation happens only at the "total tokens" or "cost (USD)" level.
2. **Provider-specific code in provider directories** -- All parsing logic, types, and DB schema definitions live under `src/providers/<name>/`. Common traits and shared infrastructure (checkpoint, mmap streaming, engine loop) remain in shared locations.
3. **Separate DB per provider** -- Cleaner isolation, simpler add/remove (delete DB directory), independent I/O, parallel reads in report mode.
4. **Sequential cold-start** -- Providers run cold-start one after another. Rayon already parallelizes within each provider, so cross-provider parallelism would just compete for the same thread pool.
5. **Add/remove configuration pattern** -- Providers are toggled individually, not set-all-at-once, preventing accidental removal.

---

## Current Architecture

```
CLI args
  → Config (settings.json + CLI overrides)
  → lib::start()
      → Database::open()           (single fjall DB)
      → DbWriter::new() + thread   (single writer, bounded channel)
      → TrackerEngine::new()
      → engine.cold_start(&ClaudeCodeParser, root_dir)
      → platform::create_watcher() + watch_directory()
      → engine.watch_loop(event_rx, stop_rx, &ClaudeCodeParser)
```

Key types in `src/common/types.rs`: `UsageEvent`, `UsageEventWithTs`, `TokenFields`, `StoredEvent`, `RollupValue`, `ModelUsageSummary`, `FileCheckpoint`, `SessionGroup`, `LogParser`, `LogParserWithTs`.

Key hardcoded Claude Code references:
- `lib.rs:112` -- `let parser = ClaudeCodeParser;`
- `lib.rs:113` -- `let root_dir = config.claude_code_root.clone();`
- `engine.rs:154` -- `let cs_parser = crate::providers::claude_code::ClaudeCodeParser;`
- `engine.rs:519-541` -- `extract_session_id()`, `extract_project_name()` (Claude Code path structure)
- `writer.rs:9` -- `use crate::engine::extract_project_name;`
- `config.rs:9` -- `pub claude_code_root: String`

---

## Target Architecture

```
CLI args
  → Config (settings.json with providers list)
  → lib::start()
      → for each selected provider:
          → provider.db_path() → Database::open()
          → DbWriter::new() + thread (one writer per provider)
      → TrackerEngine::new(provider_channels)
      → for each selected provider (sequential):
          → engine.cold_start(provider)
      → platform::create_watcher()
      → for each selected provider:
          → watch_directory(provider.watch_dirs())
      → engine.watch_loop(event_rx, stop_rx, providers)
          → route events by provider.owns_path()
```

---

## File Structure (Final State)

```
src/
├── providers/
│   ├── mod.rs                  # Provider trait, FileParser trait, registry
│   ├── claude_code/
│   │   ├── mod.rs              # ClaudeCodeProvider impl
│   │   ├── parser.rs           # ClaudeCodeParser (existing, minor changes)
│   │   └── types.rs            # Claude Code-specific TokenFields, StoredEvent, RollupValue, ColdStartParsed
│   └── codex/
│       ├── mod.rs              # CodexProvider impl
│       ├── parser.rs           # CodexParser (stateful: model tracking)
│       └── types.rs            # Codex-specific TokenFields, StoredEvent, RollupValue, ColdStartParsed
├── common/
│   └── types.rs                # Shared types: FileCheckpoint, SessionGroup, LogParser traits, ModelUsageSummary (display-level), UsageEvent (generic)
├── config.rs                   # + providers: Vec<String>
├── engine.rs                   # Generalized cold_start/watch_loop
├── writer.rs                   # Per-provider writer (generic over token type via DbOp enum)
├── lib.rs                      # Multi-provider startup
├── db.rs                       # Database (shared schema for checkpoints, dict, indexes; provider-specific keyspaces for events/rollups)
└── ... (rest unchanged)
```

---

## Phase 1: Provider Trait and Claude Code Refactor

**Goal**: Define the abstraction layer and wrap Claude Code in it. No behavior change.

### 1a. Provider trait (`src/providers/mod.rs`)

```rust
/// Trait that all providers must implement.
pub trait Provider: Send + Sync {
    /// Unique identifier (e.g., "claude_code", "codex")
    fn name(&self) -> &str;

    /// Human-readable display name (e.g., "Claude Code", "Codex CLI")
    fn display_name(&self) -> &str;

    /// Root directory for this provider's data.
    /// Returns None if the directory does not exist.
    fn root_dir(&self) -> Option<String>;

    /// Directories to register with the file watcher.
    fn watch_dirs(&self) -> Vec<String>;

    /// Whether this provider owns a given file path.
    fn owns_path(&self, path: &str) -> bool;

    /// Discover session groups for cold start.
    fn discover_sessions(&self) -> Vec<SessionGroup>;

    /// Create a per-file stateful parser for cold start.
    /// Each file gets its own instance (supports stateful parsing like Codex model tracking).
    fn create_file_parser(&self) -> Box<dyn FileParser>;

    /// The LogParser implementation for watch mode.
    fn parser(&self) -> &dyn LogParser;

    /// The LogParserWithTs implementation for watch mode.
    fn parser_with_ts(&self) -> &dyn LogParserWithTs;

    /// Extract session ID from a file path.
    fn extract_session_id(&self, path: &str) -> Option<String>;

    /// Extract project name from a file path.
    fn extract_project_name(&self, path: &str) -> Option<&str>;

    /// DB directory name for this provider (e.g., "claude_code.fjall").
    fn db_dir_name(&self) -> &str;

    /// Build a ColdStartEvent from parsed data.
    /// Provider-specific because token fields differ.
    fn build_cold_start_event(
        &self,
        parsed: &dyn std::any::Any,
        session_id: Arc<str>,
        source_file: Arc<str>,
    ) -> ColdStartEvent;

    /// Accumulate parsed cold-start data into a ModelUsageSummary.
    fn accumulate_summary(
        &self,
        summary: &mut ModelUsageSummary,
        parsed: &dyn std::any::Any,
    );
}
```

### 1b. FileParser trait (`src/providers/mod.rs`)

```rust
/// Per-file stateful parser for cold start.
/// Created fresh for each file via Provider::create_file_parser().
/// Lines within a file are processed sequentially, so &mut self is safe.
pub trait FileParser {
    /// Parse a single line during cold start.
    /// Returns Some with provider-specific parsed data,
    /// or None if the line is not a token usage event.
    fn parse_line(&mut self, line: &str) -> Option<ColdStartParsed>;
}
```

### 1c. ColdStartParsed (common)

Each provider has its own token fields, linked via enum:

```rust
pub struct ColdStartParsed {
    pub event_key: String,
    pub model: String,
    pub ts_ms: i64,
    /// Provider-specific token data.
    pub provider_data: ProviderTokenData,
    /// For display-level summary accumulation.
    pub total_input: u64,
    pub total_output: u64,
}

pub enum ProviderTokenData {
    ClaudeCode(claude_code::types::ClaudeTokenFields),
    Codex(codex::types::CodexTokenFields),
}
```

Using an enum rather than trait objects keeps things simple, avoids boxing, and is exhaustive.

### 1d. Move Claude Code-specific functions

Move from `engine.rs` to `src/providers/claude_code/mod.rs`:
- `extract_session_id()` (line 519) -- Claude Code path structure specific
- `extract_project_name()` (line 535) -- depends on `/projects/` marker

### 1e. Claude Code provider-specific types (`src/providers/claude_code/types.rs`)

```rust
/// Claude Code token fields -- stored in DB.
pub struct ClaudeTokenFields {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}
```

The existing `TokenFields`, `StoredEvent`, `RollupValue` in `common/types.rs` move here.

### 1f. Provider registry (`src/providers/mod.rs`)

```rust
pub const KNOWN_PROVIDERS: &[&str] = &["claude_code", "codex"];

pub fn create_providers(names: &[String]) -> Vec<Box<dyn Provider>> {
    names.iter().filter_map(|name| match name.as_str() {
        "claude_code" => Some(Box::new(claude_code::ClaudeCodeProvider::new()) as _),
        "codex" => Some(Box::new(codex::CodexProvider::new()) as _),
        _ => {
            eprintln!("[toki] Unknown provider: {}", name);
            None
        }
    }).collect()
}
```

---

## Phase 2: Configuration System

**Goal**: Provider selection via config file and CLI commands.

### 2a. Config changes (`src/config.rs`)

```rust
pub struct Config {
    pub providers: Vec<String>,  // NEW
    pub claude_code_root: String, // kept for backward compat
    pub db_base_dir: PathBuf,    // NEW: base dir for per-provider DBs
    pub db_path: PathBuf,        // DEPRECATED: kept for migration detection
    // ... rest unchanged
}
```

Backward compatibility:
- If `"providers"` key absent but `"claude_code_root"` exists → `providers: ["claude_code"]`
- If neither exists (fresh install) → `providers: []`

### 2b. CLI commands (`src/main.rs`)

```rust
/// Manage providers
Provider {
    #[command(subcommand)]
    command: ProviderCommands,
}

#[derive(Subcommand)]
enum ProviderCommands {
    Add { name: String },
    Remove { name: String },
    List,
}
```

**`toki provider add <name>`**: Validate against `KNOWN_PROVIDERS`, append to list (idempotent), save to settings.json.

**`toki provider remove <name>`**: Remove from list, optionally prompt to delete provider DB.

**`toki provider list`**:
```
  claude_code    Claude Code     ~/.claude          [enabled]
  codex          Codex CLI       ~/.codex           [disabled]
```

### 2c. No-provider guard

```rust
if config.providers.is_empty() {
    eprintln!("[toki] No providers configured.");
    eprintln!("[toki] Add a provider first:");
    eprintln!("[toki]   toki provider add claude_code");
    eprintln!("[toki]   toki provider add codex");
    eprintln!("[toki]   toki provider list");
    std::process::exit(1);
}
```

### 2d. Settings TUI (`src/settings.rs`)

Add "Providers" section with checkboxes. Mark as daemon-affecting setting.

---

## Phase 3: Per-Provider Database

**Goal**: Separate fjall DB per provider, one writer thread each.

### 3a. DB path scheme

```
~/.config/toki/
├── claude_code.fjall/
├── codex.fjall/
├── settings.json
├── daemon.sock
└── daemon.pid
```

### 3b. ProviderRuntime struct (`src/lib.rs`)

```rust
struct ProviderRuntime {
    provider: Box<dyn Provider>,
    db: Arc<Database>,
    db_tx: Sender<DbOp>,
    writer_handle: JoinHandle<()>,
}
```

### 3c. Multi-provider startup (`src/lib.rs`)

```rust
let providers = create_providers(&config.providers);
let mut runtimes = Vec::new();

for provider in providers {
    let db_path = config.db_base_dir.join(provider.db_dir_name());
    let db = Arc::new(Database::open(&db_path)?);
    let (db_tx, db_rx) = crossbeam_channel::bounded(1024);
    let writer = DbWriter::new(db.clone(), db_rx, retention);
    let writer_handle = std::thread::spawn(move || writer.run());
    runtimes.push(ProviderRuntime { provider, db, db_tx, writer_handle });
}

let channel_map: HashMap<String, Sender<DbOp>> = runtimes.iter()
    .map(|r| (r.provider.name().to_string(), r.db_tx.clone()))
    .collect();
```

### 3d. DbOp generalization

Token payload stored as opaque bytes (provider serializes its own types):

```rust
pub struct ColdStartEvent {
    pub ts_ms: i64,
    pub message_id: String,
    pub model: String,
    pub session_id: Arc<str>,
    pub source_file: Arc<str>,
    pub project_name: Option<Arc<str>>,
    pub token_bytes: Vec<u8>,           // Provider-serialized
    pub rollup_input: u64,              // For common rollup
    pub rollup_output: u64,
}
```

Writer stores `token_bytes` directly -- does not need to interpret token fields.

---

## Phase 4: Engine Generalization

**Goal**: Make `TrackerEngine` work with multiple providers.

### 4a. TrackerEngine changes

```rust
pub struct TrackerEngine {
    channels: HashMap<String, Sender<DbOp>>,  // provider_name -> channel
    checkpoints: HashMap<String, FileCheckpoint>,
    file_sizes: HashMap<String, u64>,
    activity: HashMap<String, FileActivity>,
    dirty: HashMap<String, String>,  // file_path -> provider_name
    sink: Box<dyn Sink>,
}
```

### 4b. Generalized cold_start

```rust
pub fn cold_start(
    &mut self,
    provider: &dyn Provider,
    db_tx: &Sender<DbOp>,
) -> Result<HashMap<String, ModelUsageSummary>, Box<dyn std::error::Error>>
```

- `provider.discover_sessions()` replaces hardcoded discovery
- `provider.create_file_parser()` per file in rayon closure replaces hardcoded `ClaudeCodeParser`
- `provider.extract_session_id(path)` replaces engine-level function
- Summary accumulation uses only display-level fields (total_input, total_output, event_count)

### 4c. Generalized watch_loop

```rust
pub fn watch_loop(
    &mut self,
    event_rx: Receiver<String>,
    stop_rx: Receiver<()>,
    providers: &[Box<dyn Provider>],
)
```

For each path: find owning provider via `provider.owns_path(path)`, route to correct channel.

### 4d. flush_dirty routing

`dirty: HashMap<String, String>` (file_path → provider_name) ensures checkpoints flush to the correct provider's channel.

---

## Phase 5: Codex CLI Provider

**Goal**: Implement the Codex parser and provider.

### 5a. New files

- `src/providers/codex/mod.rs` -- `CodexProvider` impl
- `src/providers/codex/parser.rs` -- `CodexParser` + `CodexFileParser`
- `src/providers/codex/types.rs` -- `CodexTokenFields`, `CodexStoredEvent`, `CodexRollupValue`

### 5b. Codex token types (`src/providers/codex/types.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexTokenFields {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_output_tokens: u64,
}
```

### 5c. Codex stateful FileParser (`src/providers/codex/parser.rs`)

```rust
pub struct CodexFileParser {
    last_model: String,
    session_id: Option<String>,
    cwd: Option<String>,
}
```

Pre-filter keywords: `"token_count"`, `"turn_context"`, `"session_meta"`

Parsing flow:
- `session_meta` → extract `session_id`, `cwd` → return None
- `turn_context` → update `last_model` → return None
- `event_msg` with `token_count` → extract `last_token_usage` → return ColdStartParsed

Handle `"info": null` (first token_count event) gracefully.

### 5d. CodexProvider (`src/providers/codex/mod.rs`)

```rust
impl Provider for CodexProvider {
    fn name(&self) -> &str { "codex" }
    fn display_name(&self) -> &str { "Codex CLI" }
    fn root_dir(&self) -> Option<String> { /* ~/.codex */ }
    fn watch_dirs(&self) -> Vec<String> { vec!["~/.codex/sessions"] }
    fn owns_path(&self, path: &str) -> bool { path.contains("/.codex/") }
    fn discover_sessions(&self) -> Vec<SessionGroup> { /* glob sessions/**/*.jsonl */ }
    fn db_dir_name(&self) -> &str { "codex.fjall" }
    // ...
}
```

Session ID extraction: last 36 chars of filename (UUID format).

### 5e. Project name from session content

- **Cold start**: `CodexFileParser` reads `session_meta` first line, stores `cwd`. Passed via `ColdStartEvent.project_name`.
- **Watch mode**: `CodexParser` maintains `HashMap<String, String>` (file_path → cwd). On first event, reads first line if not cached.

### 5f. Shared timestamp parser

Extract `parse_ts_to_ms` from `src/providers/claude_code/parser.rs` to a common location (e.g., `src/common/time.rs`). Both providers reuse it.

---

## Phase 6: Report Query Aggregation

**Goal**: Reports query all provider DBs and merge results.

### 6a. Multi-DB query

Run query against each provider's DB, merge results:
- **Summary merge**: combine by model name, add total_input/total_output/event_count
- **Grouped merge**: merge by (period, model)
- **Session/project list**: concatenate, deduplicate, sort
- **Provider-specific detail** (e.g., reasoning tokens): available only in provider-scoped queries

### 6b. Provider-specific reports

Future: `--provider` flag for `toki report` to show provider-specific token detail.

---

## Phase 7: Migration and Backward Compatibility

### 7a. DB migration

First run: if `toki.fjall` exists and `claude_code.fjall` does not → rename.

### 7b. Config migration

Implicit: absent `"providers"` key + existing `"claude_code_root"` → treat as `["claude_code"]`.

### 7c. Daemon reset

`toki daemon reset` deletes all `~/.config/toki/*.fjall/` directories.

---

## Implementation Order

| Step | Phase | Description | Risk |
|------|-------|-------------|------|
| 1 | 1a-1f | Provider trait + Claude Code refactor | Low -- no behavior change |
| 2 | 2a-2c | Config providers field + CLI commands + guard | Low |
| 3 | 3a-3d | Per-provider DB + multi-writer + Handle refactor | Medium -- core plumbing change |
| 4 | 4a-4d | Engine generalization (cold_start, watch_loop) | Medium -- touches hot path |
| 5 | 5a-5f | Codex parser + provider | Low -- additive |
| 6 | 6a-6b | Report query aggregation | Medium -- merge logic |
| 7 | 7a-7c | Migration + backward compat | Low |
| 8 | 2d | Settings TUI update | Low -- UI only |

Steps 1-4 are developed and tested with only Claude Code, ensuring no regression before adding Codex.

---

## Testing Strategy

### Unit tests

- Codex parser: known JSONL lines, model state tracking, null info handling, session_meta extraction
- Provider trait: mock provider for engine tests
- Config: backward-compatible loading, providers list serialization
- Migration: rename logic

### Integration tests

- Cold start with both providers enabled (temp directories)
- Watch mode: file events routed to correct provider
- Report: merged results from two DBs
- `toki provider add/remove` end-to-end

### Regression tests

- All existing Claude Code tests must pass unchanged
- Benchmark: cold start and incremental read performance must not regress

---

## Future Considerations

### Gemini CLI

JSON (not JSONL) format — `process_lines_streaming` cannot be reused directly. The Provider trait accommodates this: Gemini provider implements `discover_sessions` and `create_file_parser` differently, reading entire JSON files. Token fields: `input`, `output`, `cached`, `thoughts`, `tool`, `total`.

### Cross-provider cost aggregation

Each provider uses its own pricing model. Cross-provider "total cost" is the sum of per-provider costs. Cost calculation remains client-side via pricing tables.
