# toki usage guide

This document is task-oriented: it shows how to do things with toki. Each H2 below is tagged with one of three roles:

- **Quick reference** — lookup tables for flags, formats, settings keys.
- **Common tasks** — copy-paste examples for a specific goal.
- **How it works** — short explanations of behavior you can rely on.

## Topics

- **Run the daemon and parse providers** — `daemon`, `provider management`
- **Read your data** — `report`, `query`, `trace`, `windows`
- **Configure toki** — `settings`, `sync`, client options, output formats
- **Understand behavior** — retention, debug logging, JSONL structure, library usage

## Quick reference: commands

toki operates with a daemon/client architecture:

- **`daemon start`**: Server process. Cold start followed by file watching + TSDB storage
- **`daemon stop/restart/status`**: Daemon management
- **`daemon enable/disable`**: Install or remove login auto-start
- **`daemon reset`**: Rebuildable event DB wipe; settings and window history are preserved
- **`settings set providers --add/--remove`**: Provider management (Claude Code, Codex CLI, etc.)
- **`trace`**: Connect to daemon for real-time event streaming
- **`query`**: Top-level PromQL instant/range query; supports local and remote execution
- **`report`**: One-shot TSDB query. Retrieves data collected by the daemon
- **`windows`**: Current rate-limit gauges and persisted window history

## Build from source

```bash
git clone https://github.com/korjwl1/toki.git toki
cd toki
cargo build --release
# Binary: target/release/toki
# Add to PATH or run directly
```

Released binaries can instead be installed on macOS or Linux with
`brew tap korjwl1/tap && brew install toki`.

## Common tasks: daemon

### daemon start

```bash
toki daemon start              # Detaches to background (default)
toki daemon start --foreground # Run in foreground (for debugging)
```

Detaches to the background by default. Use `--foreground` to keep the process in the foreground for debugging.

1. Scans configured providers' session files (cold start)
2. Stores parsed events in per-provider TSDB
3. Starts filesystem watchers and configured window/sync workers
4. Starts the UDS listener for trace, report, query, and window clients

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

Stops the running daemon and restarts it. Provider roots/selection, socket path,
`window_tracking`, `retention_days`, and `window_retention_days` require a
restart. Sync and `window_polling` are hot-reloaded; display settings are read
by each new CLI process.

### daemon status

```bash
toki daemon status
```

Shows daemon running status and PID.

### daemon reset

```bash
toki daemon reset
```

If the daemon is running, stops it first, then deletes the legacy event DB and
each provider's rebuildable `<provider>.fjall` event DB. Events, indexes,
dictionaries, and checkpoints are rebuilt from provider logs on the next start.

`settings.json`, credentials, and `<provider>.windows.fjall` are deliberately
preserved. Window peaks are observations that cannot necessarily be rebuilt.

### daemon enable / disable

```bash
toki daemon enable   # install login auto-start
toki daemon disable  # remove login auto-start
```

On macOS this uses launchd; on Linux it uses a user systemd unit.

## Common tasks: provider management

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

Each provider has an event database (`~/.config/toki/<provider>.fjall`) and a
separate window-history database (`~/.config/toki/<provider>.windows.fjall`).
After adding or removing a provider, restart the daemon if it is running.

## Common tasks: trace

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

- Always outputs JSONL (no `--output-format` option; query/report use that option)
- Supports `--sink` for relaying to UDS or HTTP targets
- Includes `cost_usd` field by default (daemon loads pricing); use `--no-cost` to exclude
- Daemon must be running (`toki daemon start` first)
- Multiple clients can connect simultaneously (fan-out via condvar, 2 threads per client)
- When no clients are connected, daemon Sink processing is effectively a no-op (zero overhead)
- Exit with Ctrl+C. The daemon keeps running
- When using `--sink uds://` or `--sink http://`, spawn `toki trace` as a child process — it auto-terminates when the parent dies (SIGPIPE)

## Common tasks: report

The daemon must be running. If the daemon is down, shows "Cannot connect to toki daemon" with instructions to start it.
If the daemon is running but has no data yet (cold start in progress), shows "No data in TSDB".

### Full summary

```bash
toki report
toki report --provider claude_code            # Single provider only
toki report --start 20260301
toki report --start 20260301 --end 20260331
```

Outputs per-model token usage totals for the entire period or specified range.
By default, the response contains a separate result set for every active
provider. Token-column semantics are not merged across providers. Use
`--provider` to select one.

### Time-based grouping

```bash
toki report daily --start 20260301
toki report weekly --start 20260301
toki report weekly --start 20260301 --start-of-week tue
toki report monthly
toki report yearly
toki report hourly --start 20260301
```

All grouping subcommands accept an optional range. Use `--start` for bounded
output, especially with `hourly`, `daily`, or `weekly`.

### --start / --end format

| Format | Example | Interpretation |
|--------|---------|---------------|
| `YYYYMMDD` / `YYYY-MM-DD` | `20260301` / `2026-03-01` | `--start`: 00:00:00.000, `--end`: 23:59:59.999 |
| `YYYYMMDDhhmmss` | `20260301143000` | Exact time |
| Unix seconds | `1772323200` | Exact UTC second |
| Unix milliseconds | `1772323200123` | Exact UTC millisecond |
| RFC 3339 / ISO 8601 | `2026-03-01T14:30:00+09:00` | Offset-aware; naive values use the selected timezone |

- If `--timezone` is set, input values are interpreted as local time in that timezone and converted to UTC
- Without `--timezone`, timezone-less values are interpreted as UTC
- A date-only `--end` includes the complete final day; reversed ranges are rejected

```bash
# UTC-based
toki report daily --start 20260301

# KST-based (2026-03-01 00:00:00 KST = 2026-02-28 15:00:00 UTC)
toki report -z Asia/Seoul daily --start 20260301
```

### Session grouping

```bash
toki report --group-by-session
toki report --group-by-session --start 20260301
```

Cannot be used simultaneously with time-based subcommands (`daily`, `weekly`, etc.).

### Filtering

`--session-id`, `--project`, and `--provider` can be used with all report modes.

```bash
# Project filter (substring match)
toki report --project toki
toki report daily --start 20260301 --project ddleague
toki report monthly --project myapp

# Session filter (UUID prefix)
toki report --session-id 4de9291e
toki report --session-id 4de9 --group-by-session

# Provider filter
toki report --provider claude_code
toki report --provider codex daily --start 20260301

# Combination
toki report --session-id abc --project myapp
toki report daily --start 20260301 --session-id abc
```

All summaries and grouped reports scan the time-ordered event keyspace. Session
and project listing can use their indexes when no time filter is present.

### PromQL-style queries

Use the top-level `query` command for PromQL-inspired free queries.

#### Syntax

```text
[agg_func(] metric{filters}[bucket] [offset duration] [)] [by (dimensions)]
```

| Element | Required | Description |
|---------|----------|-------------|
| `metric` | Yes | `toki_tokens_total` (`usage` alias), `cost`, `events`, `windows`, `sessions`, `projects` |
| `{filters}` | No | `key="value"` pairs, comma-separated |
| `[bucket]` | No | Time bucket: `s`, `m`, `h`, `d`, `w` — compound ok: `2h30m` (usage only). Returns only buckets with data; empty intervals are not zero-filled. |
| `offset <dur>` | No | Shift time window back (e.g. `offset 7d`) |
| `sum\|avg\|count()` | No | Aggregation: collapse model dimension (usage only) |
| `by (dims)` | No | Group by: `model`, `session`, `project` (usage only) |

Filter keys: `model`, `session`, `project`, `provider`, `type`. Time bounds are
CLI flags, not label filters.

#### Examples

```bash
# Full usage summary
toki query 'usage'

# Model filter
toki query 'usage{model="claude-opus-4-6"}'

# 1-hour bucket + model grouping
toki query --start 20260301 'usage[1h] by (model)'

# Provider filter + model grouping
toki query 'usage{provider="codex"} by (model)'

# Session grouping + time range
toki query --start 20260301 --end 20260331 'usage by (session)'

# Project grouping
toki query 'usage{project="myapp"} by (project)'

# Multi-dimension grouping
toki query 'usage[1d] by (model, session)'

# Offset modifier — compare with previous period
toki query 'usage[1d] offset 7d'

# Aggregation functions — collapse model dimension
toki query 'sum(usage[1d])'                                    # daily total
toki query 'avg(usage[1d])'                                    # per-event average
toki query 'count(usage[1d])'                                  # event count only
toki query --start 20260301 'sum(usage[1d]) by (project)'      # per-project daily sum

# Raw events
toki query --start 20260320 'events'
toki query --start 20260301 'events{model="claude-opus-4-6"}'
toki query 'events{session="abc123"}'

# Session listing
toki query 'sessions'
toki query 'sessions{project="myapp"}'
toki query --start 20260301 'sessions'

# Project listing
toki query 'projects'
toki query 'projects{project="myapp"}'
```

#### Aggregation semantics

| Function | Token Fields | Event Count | Cost | Model Name |
|----------|-------------|-------------|------|------------|
| `sum()` | Sum across all models | Sum | Sum | `(total)` |
| `avg()` | Sum / event_count | 1 | Sum / count | `(avg/event)` |
| `count()` | 0 | Sum | 0 | `(count)` |

Without aggregation, results are broken down per model (default behavior).

#### Events output

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

## Common tasks: query

`toki query` is the PromQL entry point. Without explicit bounds it acts as an
instant/rolling query; with `--start` or `--end` it scans that range. A selector
such as `[1h]` or `[1d]` defines output buckets. For remote queries it also
supplies the default step when `--step` is omitted.

### Flags

| Flag | Description |
|------|-------------|
| `-z <IANA>` | Timezone for time interpretation |
| `-w`, `--start-of-week <day>` | Week boundary for `[1w]` buckets |
| `--remote` | Send query to toki-sync server instead of local daemon |
| `--output-format table\|json` | Output format |
| `--start`, `--end` | Explicit scan bounds |
| `--step <duration>` | Remote range-query step; overrides selector-derived step |
| `--no-cost` | Disable cost calculation |

### Examples

```bash
# Instant query (pure PromQL)
toki query "sum by (model)(toki_tokens_total[1h])"
toki query -z Asia/Seoul "sum by (model)(toki_tokens_total[1d])"

# type filter and =~ regex operator
toki query 'sum(toki_tokens_total{type=~"input|output"}[1h])'

# Remote (via sync server)
toki query --remote "sum by (model)(toki_tokens_total[1h])"

# Tuesday week boundary
toki query -w tue "sum by (model)(toki_tokens_total[1w])"

# Explicit range
toki query --start 2026-03-01 --end 2026-03-31 --step 1d \
  "sum by (model)(toki_tokens_total[1d])"

# JSON output
toki query --output-format json "toki_tokens_total[1h]"

# Without cost
toki query --no-cost "toki_tokens_total[1h]"
```

`toki report query` has been removed. Move its expression to `toki query` and
keep the range flags:

```bash
toki query --start 20260301 --end 20260331 \
  "sum by (model)(toki_tokens_total[1d])"
```

Current remote limitations: the sync server does not accept every RFC 3339
bound supported by the local parser, and server queries are capped without
pagination. Very large remote ranges can therefore be partial without a CLI
truncation warning; prefer bounded queries.

## Common tasks: rate-limit windows

Window tracking is enabled by default. Codex limits are extracted passively
from rollout JSONL; Claude limits come from an activity-gated usage/profile
poller. Window rows use a separate per-provider DB and sync when enabled.

```bash
toki windows                         # same as `windows status`
toki windows status --fresh          # bounded live revalidation
toki windows status --json
toki windows list                    # -28d through +8d anchors by default
toki windows list --start 20260801 --end 20260831
toki windows list --start 2026-08-01 --end 2026-08-31 --json
toki query windows                   # stored local history
toki query --remote windows          # merged server history
```

`window_tracking=false` disables collection and requires a restart.
`window_polling=false` only disables Claude network polling and hot-reloads.
History follows `window_retention_days` (730 days by default).

## Common tasks: settings

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

Boolean settings accept `true/false`, `on/off`, `yes/no`, and `1/0`. Invalid
IANA timezone names are rejected instead of silently becoming UTC.

| Setting | Key | Default | Daemon effect |
|---------|-----|---------|---------------|
| Providers | `providers` | auto-detected if unset | Restart |
| Claude Code root | `claude_code_root` | `~/.claude` | Restart |
| Codex CLI root | `codex_root` | `~/.codex` | Restart |
| Daemon socket | `daemon_sock` | `~/.config/toki/daemon.sock` | Restart |
| Timezone | `timezone` | empty (UTC) | Hot reload/client |
| Output format | `output_format` | `table` | Hot reload/client |
| Start of week | `start_of_week` | `mon` | Hot reload/client |
| No cost | `no_cost` | `false` | Client setting; daemon startup pricing currently unaffected |
| Event retention | `retention_days` | `0` (unlimited) | Restart |
| Window tracking | `window_tracking` | `true` | Restart |
| Claude window polling | `window_polling` | `true` | Hot reload |
| Window retention | `window_retention_days` | `730` | Restart |
| Login auto-start | `daemon_autostart` | platform state | CLI-managed |

Settings priority: **CLI args > Settings file (settings.json) > Defaults**

Known CLI mismatch: `settings set retention_days ...` currently prints that it
will hot-reload, but the writer captures its retention policy at daemon start.
Restart the daemon after changing this key.

`TOKI_HOME` overrides the home directory used for provider roots and toki state
in isolated runs. `TOKI_DEBUG` enables diagnostic logging.

## Common tasks: sync

Sync token usage across multiple devices to a central [toki-sync](https://github.com/korjwl1/toki-sync) server. Sync management lives under `toki settings sync`.

### sync enable

```bash
toki settings sync enable --server <host>
toki settings sync enable --server sync.example.com
toki settings sync enable --server 1.2.3.4 --insecure
```

Connects the daemon to a toki-sync server. Opens a browser for authentication via device code flow. No credentials are passed on the command line. Takes effect immediately via hot-reload — no daemon restart needed.

| Flag | Required | Description |
|------|----------|-------------|
| `--server <host>` | Yes | Sync server hostname or IP (no port) |
| `--sync-port <port>` | No | TCP sync port (default: 9090) |
| `--http-port <port>` | No | HTTP API port (default: 443 with TLS / 9091 without TLS) |
| `--insecure` | No | Accept self-signed TLS certificates (for IP-only servers) |
| `--no-tls` | No | Disable TLS entirely (development only) |
| `--headless` | No | Non-interactive mode (prints a URL and code to enter manually) |
| `--device-name <name>` | No | Custom device name (default: hostname) |

Credentials are stored in macOS Keychain (macOS) or `~/.config/toki/sync.json` (Linux).

### sync disable

Disables sync and clears local credentials.

```bash
toki settings sync disable              # Interactive: asks to delete remote data
toki settings sync disable --delete     # Delete this device + VM data from server
toki settings sync disable --keep       # Keep remote data (device history preserved)
```

| Flag | Behavior |
|------|----------|
| (none) | Prompts: "Delete this device's data from the server? [y/N]" |
| `--delete` | Immediately deletes device and its time-series data from the server |
| `--keep` | Server data preserved -- useful for device migration or temporary disable |

In all cases, local credentials (Keychain/sync.json) and settings are cleared. Takes effect immediately via hot-reload.

### sync status

```bash
toki settings sync status
```

Shows current sync configuration: server address, device name, connection state, and TLS mode.

### sync rename

```bash
toki settings sync rename <new-name>
```

Renames the current device on the sync server.

### sync devices

```bash
toki settings sync devices
```

Lists all devices registered under your account on the sync server.

### sync remove

```bash
toki settings sync remove <device-id>
```

Removes the selected device from the server. It will be rejected on its next
connection. Use `devices` first to obtain the ID.

### sync command summary

The complete sync command set is:

```bash
toki settings sync enable --server <host>
toki settings sync disable              # Interactive prompt
toki settings sync disable --delete     # Delete remote data
toki settings sync disable --keep       # Keep remote data
toki settings sync status
toki settings sync devices
toki settings sync rename <new-name>
toki settings sync remove <device-id>
```

### query --remote

Query server-aggregated data directly from the CLI:

```bash
toki query --remote 'sum by (model)(toki_tokens_total)'
toki query --remote 'toki_tokens_total{device="macbook-pro"}'
```

The `--remote` flag sends the PromQL query to the toki-sync server instead of the local daemon. Requires sync to be enabled.

## Quick reference: client options

| Option | Applies to | Description |
|--------|-----------|-------------|
| `--output-format table\|json` | query, report | Override output format |
| `--sink <SPEC>` | trace | Output target: `print`, `uds://<path>`, `http://<url>` (repeatable) |
| `--timezone <IANA>` / `-z` | query, report | Override timezone |
| `-w`, `--start-of-week <day>` | query | Week boundary for `[1w]` buckets |
| `--start`, `--end` | query, report, windows list | Time bounds |
| `--step <duration>` | query | Remote range-query step |
| `--remote` | query | Send query to toki-sync server |
| `--no-cost` | trace, query, report | Disable cost calculation |

### --output-format

```bash
toki report --output-format table          # default
toki report --output-format json
```

Applies to query and report; window commands use their own `--json` flag.

### --timezone / -z

```bash
toki report -z Asia/Seoul daily --start 20260301
toki report -z US/Eastern weekly --start 20260101
```

Applies to:
- `--start`/`--end` input value interpretation
- Time bucketing (date boundaries for daily/hourly grouping, etc.)

### --no-cost

```bash
toki report --no-cost
toki trace --no-cost
```

For report: skips pricing data fetch and hides the Cost column.
For trace: strips `cost_usd` field from JSONL output.

Pricing comes from the LiteLLM cache at `~/.config/toki/pricing.json`. Exact
model matches win. When an exact Claude `-fast` row is missing, known Opus fast
variants use the configured 2x provider multiplier. If a model has no published
cache-read rate, toki conservatively uses its normal input rate rather than
assuming cached input is free. With no usable cached/online price, cost is
omitted instead of guessed.

## Quick reference: output formats

### Table (default)

#### Full summary

```text
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

```text
[toki] Usage by daily
─── 2026-03-01 ───
┌───────────────────────────┬─────────┬─────────┬────────────┬──────────────┬──────────────┬────────┬─────────┐
│ Model                     ┆ Input   ┆ Output  ┆ ...        ┆ ...          ┆ ...          ┆ Events ┆ Cost    │
...
─── 2026-03-02 ───
...
```

#### Session/project listing

```text
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

#### Watch mode (real-time events, trace client)

```text
[toki] claude-opus-4-6 | session.jsonl | in:3 cc:5139 cr:9631 out:14 | $0.0112
```

### JSON (`--output-format json`)

All JSON report output is wrapped with `information` (query metadata) and `providers` (data keyed by provider name).

| Field | Description |
|-------|-------------|
| `since` / `until` | Actual data range reported by the event DB |
| `query_since` / `query_until` | User-specified `--start`/`--end` filter (null if not set) |
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

#### Watch event (JSONL, one line at a time — trace output)

```json
{"type":"event","data":{"model":"claude-opus-4-6","source":"4de9291e","provider":"Claude Code","timestamp":"2026-03-19T10:30:00.123Z","input_tokens":3,"output_tokens":14,"cache_creation_input_tokens":5139,"cache_read_input_tokens":9631,"cost_usd":0.0112}}
```

> Trace always outputs JSONL. Use `--no-cost` to exclude the `cost_usd` field.

### Provider-specific columns

Each provider has its own token column schema. Table headers and JSON keys differ per provider:

| Provider | Columns | JSON Keys |
|----------|---------|-----------|
| Claude Code | Input, Output, Cache Create, Cache Read | `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` |
| Codex CLI | Input, Output, Cached Input, Reasoning Output | `input_tokens`, `output_tokens`, `cached_input_tokens`, `reasoning_output_tokens` |

Reports return per-provider tables, each with provider-specific column headers. Multi-provider results are never merged into a single table since the column semantics differ.

### UDS/HTTP sink

UDS and HTTP sinks use the same JSON structure. Always JSON regardless of `--output-format`.

- **UDS**: NDJSON (line-by-line) transmission. If socket doesn't exist, logs error and continues
- **HTTP**: JSON POST (5s timeout). On failure, logs error and continues

## How it works: retention

Event retention is disabled by default. Window history has an independent
730-day default because it is not always reconstructable from provider logs.

| Target | Default Retention | Settings Key |
|--------|-------------------|-------------|
| events (individual events) | 0 (unlimited) | `retention_days` |
| rate-limit windows | 730 days | `window_retention_days` |

- 0 = disabled (data is not deleted)
- When enabled: runs once on daemon start + every 24 hours thereafter
- Event cleanup also reclaims time indexes and unused dictionary entries
- Window cleanup runs independently even when event retention is disabled

## How it works: debug logging

```bash
# Level 1: state transitions, events, timing, writer flush
TOKI_DEBUG=1 toki daemon start

# Level 2: Level 1 + size unchanged, no new lines skip logs
TOKI_DEBUG=2 toki daemon start
```

Example output:

```text
[toki:debug] process_file /path/to/session.jsonl — 3 lines, 1024 bytes, 2 events, Active | find_resume: 50µs, read: 120µs, total: 180µs
[toki:debug] flush_dirty — 5 checkpoints sent to writer
[toki:writer] flushed 64 events in 450µs
[toki:writer] daily retention: 150 events, 24 index, 2 windows, 12 dict entries removed (35ms)
```

## Common tasks: library usage

```toml
[dependencies]
toki = { path = "." }
```

```rust
use toki::{Config, start};
use toki::daemon::BroadcastSink;
use std::sync::Arc;

fn main() {
    let config = Config::new(); // loads defaults, then settings.json

    let broadcast = Arc::new(BroadcastSink::new());
    let handle = start(config, Box::new(broadcast.clone()))
        .expect("Failed to start");

    // ... application logic ...
    // broadcast.add_client(stream) to add trace clients

    handle.stop(); // or auto-shutdown on drop
}
```

## How it works: Claude Code JSONL structure

Claude Code stores session logs under `~/.claude/projects/<encoded-path>/`.

```text
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
