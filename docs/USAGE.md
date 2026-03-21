# toki Usage Guide

## Build from Source

```bash
cargo build --release
# Binary: target/release/toki
# Add to PATH or run directly
```

## Commands

toki operates with a daemon/client architecture:
- **`daemon start`**: Server process. Cold start followed by file watching + TSDB storage
- **`daemon stop/restart/status`**: Daemon management
- **`daemon reset`**: Full DB wipe and reinitialization
- **`settings set providers --add/--remove`**: Provider management (Claude Code, Codex CLI, etc.)
- **`trace`**: Connect to daemon for real-time event streaming
- **`report`**: One-shot TSDB query. Retrieves data collected by the daemon

## daemon

### daemon start

```bash
toki daemon start              # Detaches to background (default)
toki daemon start --foreground # Run in foreground (for debugging)
```

Detaches to the background by default. Use `--foreground` to keep the process in the foreground for debugging.

1. Scans configured providers' session files (cold start)
2. Stores parsed events in per-provider TSDB
3. Outputs total token usage summary
4. Enters FSEvents watch mode
5. Starts UDS listener (awaits trace client connections)

Daemon settings (socket path, Claude Code root, etc.) are managed via `toki settings`.

Only one daemon per DB path is allowed.
If already running, exits with `Daemon already running (PID xxx)`.

### daemon stop

```bash
toki daemon stop
```

Sends SIGTERM to the running daemon for graceful shutdown.
Cleans up PID file and socket file.

### daemon restart

```bash
toki daemon restart
```

Stops the running daemon and restarts it.
Use this command to apply settings changes from `toki settings`.

### daemon status

```bash
toki daemon status
```

Shows daemon running status and PID.

### daemon reset

```bash
toki daemon reset
```

If the daemon is running, stops it first, then completely deletes the TSDB database.
All events, rollups, checkpoints, and settings are reset.
After deletion, use `toki daemon start` to collect data from scratch.

## Provider Management

toki auto-detects `~/.claude` (Claude Code) and `~/.codex` (Codex CLI) and enables them automatically. In most cases, no configuration is needed.
To manage manually, use the TUI (`toki settings`) or CLI.

```bash
# Enable Claude Code tracking
toki settings set providers --add claude_code

# Enable Codex CLI tracking
toki settings set providers --add codex

# Disable a provider
toki settings set providers --remove codex

# List all providers + status
toki settings get providers
```

Each provider has an independent database (`~/.config/toki/<provider>.fjall`).
After adding or removing a provider, restart the daemon if it is running.

## trace

trace is a client command that connects to a running daemon via UDS to receive real-time events. It sends the `TRACE` command to the daemon and receives a JSONL stream.

```bash
# Real-time JSONL output to stdout
toki trace

# Relay to UDS or HTTP
toki trace --sink uds:///tmp/toki.sock
toki trace --sink http://localhost:8080/events

# Multi-sink (terminal + HTTP)
toki trace --sink print --sink http://localhost:8080/events

# Without cost field
toki trace --no-cost
```

- Always outputs JSONL (no `--output-format` option — that applies to report only)
- Supports `--sink` for relaying to UDS or HTTP targets
- Includes `cost_usd` field by default (daemon loads pricing); use `--no-cost` to exclude
- Daemon must be running (`toki daemon start` first)
- Multiple clients can connect simultaneously (fan-out via condvar, 2 threads per client)
- When no clients are connected, daemon Sink processing is effectively a no-op (zero overhead)
- Exit with Ctrl+C. The daemon keeps running
- When using `--sink uds://` or `--sink http://`, spawn `toki trace` as a child process — it auto-terminates when the parent dies (SIGPIPE)

## report

The daemon must be running. If the daemon is down, shows "Cannot connect to toki daemon" with instructions to start it.
If the daemon is running but has no data yet (cold start in progress), shows "No data in TSDB".

### Full Summary

```bash
toki report
toki report --provider claude_code            # Single provider only
toki report --since 20260301
toki report --since 20260301 --until 20260331
```

Outputs per-model token usage totals for the entire period or specified range.
By default, results from all active providers are merged. Use `--provider` to filter to a single provider.

### Time-based Grouping

```bash
toki report daily --since 20260301
toki report weekly --since 20260301
toki report weekly --since 20260301 --start-of-week tue
toki report monthly
toki report yearly
toki report hourly --since 20260301
```

| Subcommand | `--since` required | Note |
|------------|-------------------|------|
| `hourly` | Yes | |
| `daily` | Yes | |
| `weekly` | Yes | `--start-of-week` available |
| `monthly` | No | |
| `yearly` | No | |

`hourly`, `daily`, `weekly` may produce large output, so `--since` is required.

### --since / --until Format

| Format | Example | Interpretation |
|--------|---------|---------------|
| `YYYYMMDD` | `20260301` | `--since`: 00:00:00, `--until`: 23:59:59 |
| `YYYYMMDDhhmmss` | `20260301143000` | Exact time |

- If `--timezone` is set, input values are interpreted as local time in that timezone and converted to UTC
- Without `--timezone`, values are interpreted as UTC

```bash
# UTC-based
toki report daily --since 20260301

# KST-based (2026-03-01 00:00:00 KST = 2026-02-28 15:00:00 UTC)
toki -z Asia/Seoul report daily --since 20260301
```

### Session Grouping

```bash
toki report --group-by-session
toki report --group-by-session --since 20260301
```

Cannot be used simultaneously with time-based subcommands (`daily`, `weekly`, etc.).

### Filtering

`--session-id`, `--project`, and `--provider` can be used with all report modes.

```bash
# Project filter (substring match)
toki report --project toki
toki report daily --since 20260301 --project ddleague
toki report monthly --project myapp

# Session filter (UUID prefix)
toki report --session-id 4de9291e
toki report --session-id 4de9 --group-by-session

# Provider filter
toki report --provider claude_code
toki report --provider codex daily --since 20260301

# Combination
toki report --session-id abc --project myapp
toki report daily --since 20260301 --session-id abc
```

When filters are specified, event-level scanning is used instead of rollups (rollups lack session/project information).

### PromQL-style Queries

Use the `report query` subcommand for PromQL-inspired free queries.

#### Syntax

```
[agg_func(] metric{filters}[bucket] [offset duration] [)] [by (dimensions)]
```

| Element | Required | Description |
|---------|----------|-------------|
| `metric` | Yes | `usage`, `sessions`, `projects`, `events` |
| `{filters}` | No | `key="value"` pairs, comma-separated |
| `[bucket]` | No | Time bucket: `s`, `m`, `h`, `d`, `w` (usage only) |
| `offset <dur>` | No | Shift time window back (e.g. `offset 7d`) |
| `sum\|avg\|count()` | No | Aggregation: collapse model dimension (usage only) |
| `by (dims)` | No | Group by: `model`, `session`, `project` (usage only) |

Filter keys: `model`, `session`, `project`, `provider`, `since`, `until`

#### Examples

```bash
# Full usage summary
toki report query 'usage'

# Model filter
toki report query 'usage{model="claude-opus-4-6"}'

# 1-hour bucket + model grouping
toki report query 'usage{since="20260301"}[1h] by (model)'

# Provider filter + model grouping
toki report query 'usage{provider="codex"} by (model)'

# Session grouping + time range
toki report query 'usage{since="20260301", until="20260331"} by (session)'

# Project grouping
toki report query 'usage{project="myapp"} by (project)'

# Multi-dimension grouping
toki report query 'usage[1d] by (model, session)'

# Offset modifier — compare with previous period
toki report query 'usage[1d] offset 7d'

# Aggregation functions — collapse model dimension
toki report query 'sum(usage[1d])'                                    # daily total
toki report query 'avg(usage[1d])'                                    # per-event average
toki report query 'count(usage[1d])'                                  # event count only
toki report query 'sum(usage{since="20260301"}[1d]) by (project)'     # per-project daily sum

# Raw events
toki report query 'events{since="20260320"}'
toki report query 'events{model="claude-opus-4-6", since="20260301"}'
toki report query 'events{session="abc123"}'

# Session listing
toki report query 'sessions'
toki report query 'sessions{project="myapp"}'
toki report query 'sessions{since="20260301"}'

# Project listing
toki report query 'projects'
toki report query 'projects{project="myapp"}'
```

#### Aggregation Semantics

| Function | Token Fields | Event Count | Cost | Model Name |
|----------|-------------|-------------|------|------------|
| `sum()` | Sum across all models | Sum | Sum | `(total)` |
| `avg()` | Sum / event_count | 1 | Sum / count | `(avg/event)` |
| `count()` | 0 | Sum | 0 | `(count)` |

Without aggregation, results are broken down per model (default behavior).

#### Events Output

The `events` metric returns individual API call records:

```json
{
  "type": "events",
  "data": [
    {
      "timestamp": "2026-03-20T10:30:00",
      "model": "claude-opus-4-6",
      "session": "4de9291e-...",
      "project": "myapp",
      "input_tokens": 100,
      "output_tokens": 50,
      "cache_creation_input_tokens": 0,
      "cache_read_input_tokens": 0,
      "cost_usd": 0.003
    }
  ]
}
```

## settings

`toki settings` opens a cursive TUI settings page. All settings are stored in `~/.config/toki/settings.json`.

```bash
# Configure via TUI
toki settings

# Non-interactive CLI
toki settings set claude_code_root ~/.claude
toki settings set timezone Asia/Seoul
toki settings get timezone
toki settings list
```

When daemon-affecting settings (`claude_code_root`, `codex_root`, `daemon_sock`, `retention_days`, `rollup_retention_days`) are changed and the daemon is running, you'll be prompted to restart.

| Setting | Key | Default | Daemon Effect |
|---------|-----|---------|---------------|
| Providers | `providers` | `[]` | Yes |
| Claude Code Root | `claude_code_root` | `~/.claude` | Yes |
| Codex CLI Root | `codex_root` | `~/.codex` | Yes |
| Daemon Socket | `daemon_sock` | `~/.config/toki/daemon.sock` | Yes |
| Timezone | `timezone` | (empty = UTC) | No |
| Output Format | `output_format` | `table` | No |
| Start of Week | `start_of_week` | `mon` | No |
| No Cost | `no_cost` | `false` | No |
| Retention Days | `retention_days` | `0` (unlimited) | Yes |
| Rollup Retention Days | `rollup_retention_days` | `0` (unlimited) | Yes |

Settings priority: **CLI args > Settings file (settings.json) > Defaults**

Environment variables are not used (except `TOKI_DEBUG`).

## Client Options

| Option | Applies to | Description |
|--------|-----------|-------------|
| `--output-format table\|json` | report | Override output format |
| `--sink <SPEC>` | trace | Output target: `print`, `uds://<path>`, `http://<url>` (repeatable) |
| `--timezone <IANA>` / `-z` | report | Override timezone |
| `--no-cost` | trace, report | Disable cost calculation |

### --output-format (report only)

```bash
toki report --output-format table          # default
toki report --output-format json
```

Applies only to report's `print` output.

### --timezone / -z

```bash
toki report -z Asia/Seoul daily --since 20260301
toki report -z US/Eastern weekly --since 20260101
```

Applies to:
- `--since`/`--until` input value interpretation
- Time bucketing (date boundaries for daily/hourly grouping, etc.)

### --no-cost

```bash
toki report --no-cost
toki trace --no-cost
```

For report: skips pricing data fetch and hides the Cost column.
For trace: strips `cost_usd` field from JSONL output.

## Output Formats

### Table (default)

#### Full Summary

```
[toki] Token Usage Summary
┌───────────────────────────┬─────────┬─────────┬────────────┬──────────────┬──────────────┬────────┬─────────┐
│ Model                     ┆ Input   ┆ Output  ┆ Cache      ┆ Cache        ┆ Total        ┆ Events ┆ Cost    │
│                           ┆         ┆         ┆ Create     ┆ Read         ┆ Tokens       ┆        ┆ (USD)   │
╞═══════════════════════════╪═════════╪═════════╪════════════╪══════════════╪══════════════╪════════╪═════════╡
│ claude-opus-4-6           ┆ 1,234   ┆ 4,321   ┆ 56,789     ┆ 98,765       ┆ 161,109      ┆ 42     ┆ $1.21   │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┤
│ claude-haiku-4-5-20251001 ┆ 567     ┆ 2,100   ┆ 12,345     ┆ 34,567       ┆ 49,579       ┆ 18     ┆ $0.023  │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┤
│ Total                     ┆ 1,801   ┆ 6,421   ┆ 69,134     ┆ 133,332      ┆ 210,688      ┆ 60     ┆ $1.23   │
└───────────────────────────┴─────────┴─────────┴────────────┴──────────────┴──────────────┴────────┴─────────┘
```

#### Grouping (daily, weekly, ...)

```
[toki] Usage by daily
─── 2026-03-01 ───
┌───────────────────────────┬─────────┬─────────┬────────────┬──────────────┬──────────────┬────────┬─────────┐
│ Model                     ┆ Input   ┆ Output  ┆ ...        ┆ ...          ┆ ...          ┆ Events ┆ Cost    │
...
─── 2026-03-02 ───
...
```

#### Session/Project Listing

```
[toki] sessions (3)
┌──────────────────────────────────────┐
│ Session ID                           │
╞══════════════════════════════════════╡
│ 4de9291e-061e-414a-85cb-de615826aded │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┤
│ db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┤
│ f1273bff-d1d8-45ae-a85e-624658132804 │
└──────────────────────────────────────┘
```

#### Watch Mode (real-time events, trace client)

```
[toki] claude-opus-4-6 | session.jsonl | in:3 cc:5139 cr:9631 out:14 | $0.0112
```

### JSON (`--output-format json`)

All JSON report output is wrapped with `information` (query metadata) and `providers` (data keyed by provider name).

| Field | Description |
|-------|-------------|
| `since` / `until` | Actual data range in TSDB (earliest/latest rollup timestamp, O(1)) |
| `query_since` / `query_until` | User-specified `--since`/`--until` filter (null if not set) |
| `timezone` | Timezone used for interpretation (null = UTC) |
| `start_of_week` | Week start day for weekly grouping |
| `generated_at` | When the report was generated |

#### Summary

```json
{
  "information": {
    "type": "summary",
    "since": "2026-01-15T00:00:00Z",
    "until": "2026-03-21T14:00:00Z",
    "query_since": null,
    "query_until": null,
    "timezone": null,
    "start_of_week": "mon",
    "generated_at": "2026-03-21T15:30:00Z"
  },
  "providers": {
    "claude_code": [
      {
        "model": "claude-opus-4-6",
        "input_tokens": 1234,
        "output_tokens": 4321,
        "cache_creation_input_tokens": 56789,
        "cache_read_input_tokens": 98765,
        "total_tokens": 161109,
        "events": 42,
        "cost_usd": 1.2345
      }
    ]
  }
}
```

#### Grouped

```json
{
  "information": {
    "type": "daily",
    "since": "2026-01-15T00:00:00Z",
    "until": "2026-03-21T14:00:00Z",
    "query_since": "20260301",
    "query_until": null,
    "timezone": "Asia/Seoul",
    "start_of_week": "mon",
    "generated_at": "2026-03-21T15:30:00Z"
  },
  "providers": {
    "claude_code": [
      {
        "period": "2026-03-01",
        "usage_per_models": [
          {
            "model": "claude-opus-4-6",
            "input_tokens": 1234,
            "output_tokens": 4321,
            "cache_creation_input_tokens": 56789,
            "cache_read_input_tokens": 98765,
            "total_tokens": 161109,
            "events": 42,
            "cost_usd": 1.2345
          }
        ]
      }
    ]
  }
}
```

#### List (sessions/projects)

```json
{
  "information": {
    "type": "sessions",
    "since": "2026-01-15T00:00:00Z",
    "until": "2026-03-21T14:00:00Z",
    "query_since": null,
    "query_until": null,
    "timezone": null,
    "start_of_week": "mon",
    "generated_at": "2026-03-21T15:30:00Z"
  },
  "providers": {
    "claude_code": [
      "4de9291e-061e-414a-85cb-de615826aded",
      "db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b"
    ]
  }
}
```

#### Watch Event (JSONL, one line at a time — trace output)

```json
{"type":"event","data":{"model":"claude-opus-4-6","source":"4de9291e","provider":"Claude Code","timestamp":"2026-03-19T10:30:00.123Z","input_tokens":3,"output_tokens":14,"cache_creation_input_tokens":5139,"cache_read_input_tokens":9631,"cost_usd":0.0112}}
```

> Trace always outputs JSONL. Use `--no-cost` to exclude the `cost_usd` field.

### Provider-specific Columns

Each provider has its own token column schema. Table headers and JSON keys differ per provider:

| Provider | Columns | JSON Keys |
|----------|---------|-----------|
| Claude Code | Input, Output, Cache Create, Cache Read | `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` |
| Codex CLI | Input, Output, Cached Input, Reasoning Output | `input_tokens`, `output_tokens`, `cached_input_tokens`, `reasoning_output_tokens` |

Reports return per-provider tables, each with provider-specific column headers. Multi-provider results are never merged into a single table since the column semantics differ.

### UDS/HTTP Sink

UDS and HTTP sinks use the same JSON structure. Always JSON regardless of `--output-format`.

- **UDS**: NDJSON (line-by-line) transmission. If socket doesn't exist, logs error and continues
- **HTTP**: JSON POST (5s timeout). On failure, logs error and continues

## Retention

Disabled by default. Configure retention periods via `toki settings` to enable.

| Target | Default Retention | Settings Key |
|--------|-------------------|-------------|
| events (individual events) | 0 (unlimited) | `retention_days` |
| rollups (hourly aggregation) | 0 (unlimited) | `rollup_retention_days` |

- 0 = disabled (data is not deleted)
- When enabled: runs once on daemon start + every 24 hours thereafter
- Recommend keeping rollups longer than events: reports remain available after events are deleted

## Debug Logging

```bash
# Level 1: state transitions, events, timing, writer flush
TOKI_DEBUG=1 toki daemon start

# Level 2: Level 1 + size unchanged, no new lines skip logs
TOKI_DEBUG=2 toki daemon start
```

Example output:
```
[toki:debug] process_file /path/to/session.jsonl — 3 lines, 1024 bytes, 2 events, Active | find_resume: 50µs, read: 120µs, total: 180µs
[toki:debug] flush_dirty — 5 checkpoints sent to writer
[toki:writer] flushed 64 events, 3 rollups in 450µs
[toki:writer] retention cleanup: 150 events, 12 rollups deleted (35ms)
```

## Library Usage

```toml
[dependencies]
toki = { path = "." }
```

```rust
use toki::{Config, start};
use toki::daemon::BroadcastSink;
use std::sync::Arc;

fn main() {
    let config = Config::new(); // loads defaults, then DB settings

    let broadcast = Arc::new(BroadcastSink::new());
    let handle = start(config, Box::new(broadcast.clone()))
        .expect("Failed to start");

    // ... application logic ...
    // broadcast.add_client(stream) to add trace clients

    handle.stop(); // or auto-shutdown on drop
}
```

## Claude Code JSONL Structure

Claude Code stores session logs under `~/.claude/projects/<encoded-path>/`.

```
~/.claude/projects/-Users-user-Documents-project/
├── 4de9291e-061e-414a-85cb-de615826aded.jsonl        # Parent session
├── 4de9291e-061e-414a-85cb-de615826aded/
│   └── subagents/
│       └── agent-aed1da92cc2e4e9e7.jsonl             # Subagent
└── db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b.jsonl        # Another session
```

Parsed line types:
- `type: "assistant"` — extracts 4 token types from `message.usage`
- `type: "user"`, `type: "file-history-snapshot"` — ignored

Subagent tokens are not included in the parent and are recorded in separate files.
See `docs/claude-code-jsonl-format.md` for detailed JSONL format.

> **Note:** Codex CLI also uses a similar JSONL format but is handled by a separate parser. See `docs/codex-cli-analysis.md` for detailed Codex data format.
