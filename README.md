<p align="center">
  <img src="assets/logo.png" alt="toki logo" width="160" />
</p>

<h1 align="center">toki</h1>

<p align="center">
  <b>Invisible token usage tracker for AI CLI tools</b><br>
  Built in Rust. Daemon-powered. 5 MB idle. Reports in 13 ms. Your workflow never notices.
</p>

<p align="center">
  <sub><b>toki</b> = <b>to</b>ken <b>i</b>nspector — sounds like <i>tokki</i> (토끼, rabbit in Korean). Fast and light, just like one.</sub>
</p>

<p align="center">
  <a href="README.ko.md">🇰🇷 한국어</a>
</p>

---

> **Engineered, not just coded.** In the age of vibe coding, toki stands apart — every architectural decision was made by a professional systems engineer who knows exactly why each piece exists. The TSDB schema, rollup-on-write strategy, xxHash3 checkpoint recovery, 4-thread daemon model — all designed with intent, built with precision.

---

## Performance

Not just fast — **lightweight enough to forget it's running.** toki sits at 5 MB idle, near-zero CPU, and answers any report in 13 ms. ccusage and zzusage spike your CPU and memory every time you ask a question, because they re-read everything from scratch. toki doesn't. It indexes once, then gets out of your way.

Benchmarked against [ccusage](https://github.com/ryoppippi/ccusage) (Node.js) and [zzusage](https://github.com/nickarellano/zzusage) (Zig) on the same dataset, disk cache purged before each run.

### Report Speed (indexed query vs full re-scan)

toki report is **~13 ms fixed** regardless of data size (UDS query → TSDB rollup lookup).
ccusage and zzusage re-read every file from scratch, every time.

| Data Size | toki | ccusage | zzusage | vs ccusage | vs zzusage |
|-----------|------|---------|---------|------------|------------|
| 100 MB | **0.013 s** | 2.37 s | 0.12 s | **182x** faster | **9x** faster |
| 500 MB | **0.013 s** | 6.05 s | 0.35 s | **465x** faster | **27x** faster |
| 1 GB | **0.013 s** | 11.07 s | 0.65 s | **851x** faster | **50x** faster |
| 2 GB | **0.013 s** | 21.73 s | 1.22 s | **1,672x** faster | **94x** faster |

### Cold Start (full index build)

toki parses **and** indexes into a TSDB simultaneously — yet still outruns tools that only parse.

| Data Size | toki | ccusage | zzusage | vs ccusage | vs zzusage |
|-----------|------|---------|---------|------------|------------|
| 100 MB | 0.11 s | 2.37 s | 0.12 s | **21x** | ~1.0x |
| 500 MB | 0.39 s | 6.05 s | 0.35 s | **16x** | ~0.9x |
| 1 GB | 0.78 s | 11.07 s | 0.65 s | **14x** | ~0.8x |
| 2 GB | 1.54 s | 21.73 s | 1.22 s | **14x** | ~0.8x |

> **Why ~1.0x vs zzusage matters:** toki does *strictly more work* per line — TSDB writes (fjall LSM-tree inserts), rollup aggregation, checkpoint persistence, and JSON schema validation. zzusage skips all of this: no DB, no validation, just raw parsing. Despite the extra workload, toki matches zzusage in wall-clock time. The validation gap also has a practical consequence: zzusage accepts any content without structural checks, making it trivial to feed crafted JSONL that inflates or fabricates usage numbers. toki validates every record before it reaches the TSDB, so tampered data is rejected at parse time.

### Memory & CPU

| Data Size | toki (cold start) | toki (idle) | ccusage | zzusage |
|-----------|-------------------|-------------|---------|---------|
| 500 MB | 83 MB | **5–11 MB** | 126 MB | 613 MB |
| 2 GB | 161 MB | **5–11 MB** | 126 MB | 2,311 MB |

- **toki** — streaming per-file with mmap zero-copy during cold start. After indexing, the daemon drops to 5–11 MB and ~0% CPU. It watches for changes via FSEvents (kernel-level, zero polling) and only wakes when Claude Code writes new lines. Reports query the TSDB in 13 ms and exit at ~5 MB.
- **ccusage** — Node.js heap capped at ~126 MB, sequential with GC. No idle state — every invocation pays full cost.
- **zzusage** — loads all events into memory. 2 GB data → 2.3 GB RAM. No idle state. Will OOM on larger datasets.

ccusage and zzusage are batch tools. Every time you ask a question, they re-read everything from scratch — 126 MB to 2.3 GB of RAM, seconds to minutes of CPU, competing with your editor, compiler, and AI agent. toki pays that cost once, then sits at 5 MB in the background. Nothing competes for resources.

> Measured on Apple M1 MacBook Air (8 GB RAM), macOS, power saving off.
> Reproduce: `sudo -v && python3 benches/benchmark.py run --purge --tool all`

---

## How It Works

Docker-like daemon/client architecture:

```
toki daemon start     # always-on server   (≈ dockerd)
toki trace            # real-time stream    (≈ docker logs -f)
toki report           # instant TSDB query  (≈ docker ps)
```

- **daemon** — watches Claude Code JSONL session logs via FSEvents, parses events, writes to an embedded TSDB (fjall). Zero sink overhead when no trace clients are connected.
- **trace** — connects to the daemon over UDS for real-time event streaming. Supports `print`, `uds://`, `http://` sinks.
- **report** — sends a query to the daemon over UDS, gets results from the TSDB. Always fast, always indexed.

---

## Quick Start

```bash
# Build
cargo build --release
# Binary: target/release/toki — add to PATH or run directly

# 1. Start the daemon (foreground, Ctrl+C to stop)
toki daemon start

# 2. In another terminal — real-time event stream
toki trace

# 3. Reports (instant TSDB queries)
toki report
toki report daily --since 20260301
toki report monthly

# 4. PromQL-style queries
toki report query 'usage{model="claude-opus-4-6"}[1h] by (model)'
toki report query 'sessions{project="myapp"}'
```

---

## Commands

### Daemon

```bash
toki daemon start       # Start (foreground)
toki daemon stop        # Stop
toki daemon restart     # Restart (reload settings)
toki daemon status      # Check status
toki daemon reset       # Wipe DB + reinitialize
```

### Report

```bash
# Summary
toki report
toki report --since 20260301 --until 20260331

# Time grouping
toki report daily --since 20260301
toki report weekly --since 20260301 --start-of-week tue
toki report monthly
toki report yearly
toki report hourly --from-beginning

# Session/project filters
toki report --group-by-session
toki report --project toki
toki report --session-id 4de9291e

# PromQL-style queries
toki report query 'usage{model="claude-opus-4-6"}[1h] by (model)'
toki report query 'usage{session="4de9", since="20260301"} by (session)'
toki report query 'sessions{project="myapp"}'
toki report query 'projects'

# Options
toki -z Asia/Seoul report daily --since 20260301   # timezone
toki --no-cost report                               # skip cost
```

<details>
<summary>Report options reference</summary>

| Option | Description |
|--------|-------------|
| *(no subcommand)* | Total summary (`--since`/`--until` optional) |
| `daily\|weekly\|monthly\|yearly\|hourly` | Time-based grouping |
| `query '<PROMQL>'` | PromQL-style free query |
| `--since YYYYMMDD[hhmmss]` | Start time (inclusive, `>=`) |
| `--until YYYYMMDD[hhmmss]` | End time (inclusive, `<=`) |
| `--from-beginning` | Allow full grouping without `--since` |
| `--group-by-session` | Group by session (mutually exclusive with time subcommand) |
| `--session-id <PREFIX>` | Filter by session UUID prefix |
| `--project <NAME>` | Filter by project directory substring |
| `--start-of-week mon\|tue\|...\|sun` | Only for `weekly` |

</details>

<details>
<summary>PromQL query syntax</summary>

```
metric{filters}[bucket] by (dimensions)
```

| Element | Description | Example |
|---------|-------------|---------|
| metric | `usage`, `sessions`, `projects` | `usage` |
| filters | `key="value"` pairs, comma-separated | `{model="claude-opus-4-6", since="20260301"}` |
| bucket | Time bucket (s/m/h/d/w) | `[1h]`, `[5m]`, `[1d]` |
| dimensions | Group by (model/session/project) | `by (model, session)` |

Filter keys: `model`, `session`, `project`, `since`, `until`

</details>

### Trace

```bash
toki trace                                              # Default (print to terminal)
toki trace --sink print --sink http://localhost:8080     # Multi-sink
```

### Settings

```bash
toki settings                              # Open TUI (cursive)
toki settings set claude_code_root /path   # Set individual value
toki settings get timezone                 # Get value
toki settings list                         # List all
```

<details>
<summary>Settings reference</summary>

| Setting | Description | Default |
|---------|-------------|---------|
| Claude Code Root | Claude Code root directory | `~/.claude` |
| Daemon Socket | Daemon UDS socket path | `~/.config/toki/daemon.sock` |
| Timezone | IANA timezone (empty = UTC) | *(none)* |
| Output Format | Default output format | `table` |
| Start of Week | Weekly report start day | `mon` |
| No Cost | Disable cost calculation | `false` |
| Retention Days | Event retention (0 = unlimited) | `0` |
| Rollup Retention Days | Rollup retention (0 = unlimited) | `0` |

Priority: **CLI args > settings.json > defaults**

</details>

### Client Options (trace / report)

| Option | Description |
|--------|-------------|
| `--output-format table\|json` | Override output format |
| `--sink <SPEC>` | Output target (repeatable) |
| `--timezone <IANA>` / `-z` | Override timezone |
| `--no-cost` | Disable cost calculation |

---

## Documentation

| Document | Description |
|----------|-------------|
| **[Architecture & Design](docs/DESIGN.md)** | Daemon threads, TSDB schema, rollup strategy, checkpoint recovery, data flow |
| **[Usage Guide](docs/USAGE.md)** | Detailed command reference, output formats, library API, examples |
| **[JSONL Format Reference](docs/claude-code-jsonl-format.md)** | Claude Code JSONL structure, line types, parsing optimizations |
| **[Benchmark Details](benches/COMPARISON.md)** | Full comparison methodology, architecture analysis, scaling predictions |

---

## Cost Calculation

All outputs include estimated cost (USD) per model, sourced from [LiteLLM](https://github.com/BerriAI/litellm) community pricing.

- **First run**: downloads LiteLLM JSON → extracts Claude model prices → caches to `~/.config/toki/pricing.json`
- **Subsequent runs**: HTTP ETag conditional request → 304 if unchanged (~50 ms, no body)
- **Offline**: uses cached data; if no cache, cost column is omitted
- **`--no-cost`**: skips price fetch entirely

---

## Tech Stack

| Purpose | Choice | Rationale |
|---------|--------|-----------|
| Database | fjall 3.x | Pure Rust LSM-tree, fits TSDB keyspace model |
| Concurrency | std::thread + crossbeam-channel | No async runtime conflicts, library-safe |
| Parallel scan | rayon | Cold start parallel file processing |
| File watching | notify 6.x | macOS FSEvents integration |
| Serialization | bincode (DB), serde_json (JSONL) | Minimal binary overhead |
| Hashing | xxhash-rust 0.8 (xxh3) | Checkpoint line identification (30 GB/s) |
| HTTP | ureq 2.x | Synchronous, ETag conditional requests |
| CLI | clap 4.x | Subcommands, global options |
| Tables | comfy-table 7.1 | Unicode table rendering |
| IPC | Unix Domain Socket | Daemon-client NDJSON streaming |

---

## Project Structure

```
src/
├── lib.rs                          # Public API: start(), Handle
├── main.rs                         # CLI binary (clap)
├── config.rs                       # Config + file-based settings
├── db.rs                           # fjall wrapper (7 keyspaces)
├── engine.rs                       # TrackerEngine: cold_start + watch_loop
├── writer.rs                       # DB writer thread (DbOp channel)
├── query.rs                        # TSDB query engine (report)
├── query_parser.rs                 # PromQL-style query parser
├── retention.rs                    # Data retention policy
├── checkpoint.rs                   # Reverse-scan, xxHash3 matching
├── pricing.rs                      # LiteLLM price fetch, ETag caching
├── settings.rs                     # Cursive TUI settings
├── common/types.rs                 # Shared types & traits
├── daemon/                         # Daemon server components
│   ├── broadcast.rs                # BroadcastSink (zero-overhead fan-out)
│   ├── listener.rs                 # UDS accept loop
│   └── pidfile.rs                  # PID file management
├── sink/                           # Output abstraction (Sink trait)
│   ├── print.rs                    # PrintSink (table/json → stdout)
│   ├── uds.rs                      # UdsSink (NDJSON → UDS)
│   └── http.rs                     # HttpSink (JSON POST)
├── providers/claude_code/parser.rs # JSONL parsing + session discovery
└── platform/macos/mod.rs           # macOS FSEvents watcher
```

---

## License

[FSL-1.1-Apache-2.0](LICENSE)
