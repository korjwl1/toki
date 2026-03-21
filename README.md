<p align="center">
  <img src="assets/logo.png" alt="toki logo" width="160" />
</p>

<h1 align="center">toki</h1>

<p align="center">
  <b>Invisible token usage tracker for AI CLI tools</b><br>
  Built in Rust. Daemon-powered. 5 MB idle. Reports in 7 ms. Your workflow never notices.
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

## Who is this for?

- **Tired of ccusage freezing your terminal?** toki is 14x faster on cold start and 1,700x faster on reports. Your terminal never hangs again — reports come back in 7 ms, not 20 seconds.

- **Want deeper analysis than "total tokens"?** toki gives you per-model, per-session, per-project, per-day breakdowns with PromQL-style queries. Filter by time range, group by any dimension, track costs across multiple AI tools — all from a single command.

- **Don't want to set up OpenTelemetry?** No collector, no config files, no environment variables. Install toki, run it, done. It reads your existing session files directly — including months of historical data from before you installed it.

- **Using multiple AI CLI tools?** toki tracks Claude Code and Codex CLI in a single unified view. Add a provider, and all your token usage is merged. Filter by `--provider` when you need per-tool breakdown.

---

## Performance

Not just fast — **lightweight enough to forget it's running.** toki sits at 5 MB idle, near-zero CPU, and answers any report in 7 ms. ccusage and zzusage spike your CPU and memory every time you ask a question, because they re-read everything from scratch. toki doesn't. It indexes once, then gets out of your way.

Benchmarked against [ccusage](https://github.com/ryoppippi/ccusage) (Node.js) and [zzusage](https://github.com/joelreymont/zzusage) (Zig) on the same dataset, disk cache purged before each run.

### Cold Start (full index build)

toki parses **and** indexes into a TSDB simultaneously — **14x faster** than ccusage, **similar speed** to zzusage but with **93% less memory**.

> In normal operation, toki resumes from its last checkpoint and only processes new data — so subsequent cold starts are significantly faster.

<p align="center">
  <img src="docs/bench_cold_start.png" alt="Cold Start Benchmark" width="900" />
</p>

<details>
<summary>Cold Start detailed data</summary>

#### Execution Time

| Data Size | toki | ccusage | zzusage | toki vs ccusage |
|-----------|------|---------|---------|-----------------|
| 100 MB | **0.11 s** | 2.38 s | 0.13 s | **21x** faster |
| 200 MB | **0.16 s** | 3.09 s | 0.18 s | **19x** faster |
| 300 MB | **0.27 s** | 4.47 s | 0.27 s | **16x** faster |
| 400 MB | **0.31 s** | 5.07 s | 0.32 s | **16x** faster |
| 500 MB | **0.39 s** | 6.06 s | 0.40 s | **15x** faster |
| 1 GB | **0.78 s** | 10.88 s | 0.76 s | **14x** faster |
| 2 GB | **1.54 s** | 21.53 s | 1.41 s | **14x** faster |

#### Peak Memory

| Data Size | toki | ccusage | zzusage |
|-----------|------|---------|---------|
| 100 MB | 37 MB | 126 MB | 165 MB |
| 200 MB | 38 MB | 127 MB | 246 MB |
| 300 MB | 67 MB | 127 MB | 421 MB |
| 400 MB | 69 MB | 127 MB | 492 MB |
| 500 MB | 71 MB | 126 MB | 615 MB |
| 1 GB | 119 MB | 127 MB | 1,209 MB |
| 2 GB | 166 MB | 126 MB | **2,311 MB** |

> **Why ~1.0x vs zzusage matters:** toki does *strictly more work* per line — TSDB writes (fjall LSM-tree inserts), rollup aggregation, checkpoint persistence, and JSON schema validation. zzusage skips all of this: no DB, no validation, just raw parsing. Despite the extra workload, toki matches zzusage in wall-clock time. The validation gap also has a practical consequence: zzusage accepts any content without structural checks, making it trivial to feed crafted JSONL that inflates or fabricates usage numbers. toki validates every record before it reaches the TSDB, so tampered data is rejected at parse time.

</details>

### Report Speed (indexed TSDB query vs full re-scan)

toki report answers in **~7 ms** regardless of data size — **1,742x faster** than ccusage at 2 GB.

<p align="center">
  <img src="docs/bench_report.png" alt="Report Benchmark" width="900" />
</p>

<details>
<summary>Report detailed data</summary>

#### Execution Time

| Data Size | toki (warm) | toki (cold disk) | ccusage | zzusage | warm vs ccusage | warm vs zzusage |
|-----------|-------------|-----------------|---------|---------|-----------------|-----------------|
| 100 MB | **0.007 s** | 0.16 s | 2.38 s | 0.13 s | **358x** | **20x** |
| 200 MB | **0.007 s** | 0.15 s | 3.09 s | 0.18 s | **435x** | **25x** |
| 300 MB | **0.007 s** | 0.15 s | 4.47 s | 0.27 s | **602x** | **37x** |
| 400 MB | **0.008 s** | 0.14 s | 5.07 s | 0.32 s | **658x** | **41x** |
| 500 MB | **0.008 s** | 0.16 s | 6.06 s | 0.40 s | **785x** | **51x** |
| 1 GB | **0.009 s** | 0.15 s | 10.88 s | 0.76 s | **1,153x** | **81x** |
| 2 GB | **0.012 s** | 0.17 s | 21.53 s | 1.41 s | **1,742x** | **114x** |

#### Peak Memory

| Data Size | toki (warm) | toki (cold disk) | ccusage | zzusage |
|-----------|-------------|-----------------|---------|---------|
| 100 MB | 5 MB | 8 MB | 126 MB | 165 MB |
| 500 MB | 5 MB | 8 MB | 126 MB | 615 MB |
| 1 GB | 5 MB | 8 MB | 127 MB | 1,209 MB |
| 2 GB | **10 MB** | 10 MB | 126 MB | **2,311 MB** |

#### Peak CPU

| Data Size | toki (warm) | toki (cold disk) | ccusage | zzusage |
|-----------|-------------|-----------------|---------|---------|
| 100 MB | 0% | 14% | 101% | 20% |
| 500 MB | 0% | 18% | 100% | 76% |
| 1 GB | 1% | 18% | 100% | 102% |
| 2 GB | 0% | 12% | 101% | 122% |

</details>

### Idle Footprint

After cold start, toki vanishes from your system's resource radar. Zero burden, always ready.

| CPU | Memory | DB Size |
|-----|--------|---------|
| **~0%** | **5 MB** | **~3% of source data** (2 GB sessions → 64 MB TSDB) |

- **toki** — rayon parallel processing across all CPU cores with mmap zero-copy and per-file streaming. Despite doing maximum parallelism, memory stays flat because each file is streamed and discarded — no accumulation. After cold start the daemon drops to 5 MB and ~0% CPU, watching for changes via FSEvents (kernel-level, zero polling) and only waking when new lines are written.
- **ccusage** — processes one file at a time, synchronous and blocking. The 126 MB looks modest on paper, but it means your terminal hangs for seconds to minutes on every invocation while Node.js chews through every file sequentially. No parallelism, no incremental processing — just a long blocking wait, every time.
- **zzusage** — loads every event from every file into memory before doing anything. Fast parsing, but 2 GB of data means 2.3 GB of RAM consumed at once. On larger datasets it simply OOMs.

toki is the only tool with an idle state. ccusage and zzusage pay full resource cost on every run — your editor, compiler, and AI agent all compete for the same CPU and memory. toki pays that cost once, drops to 5 MB, and stays out of the way.

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

- **daemon** — watches session logs from configured providers (Claude Code, Codex CLI) via FSEvents, parses events, writes to per-provider embedded TSDBs (fjall). 4 base threads + 2 per connected trace client. Zero sink overhead when no trace clients are connected. UDS protocol is command-based: clients send `TRACE\n` or `REPORT\n` as the first line.
- **trace** — connects to the daemon over UDS for real-time event streaming. Always outputs JSONL to stdout (no sink/format options).
- **report** — sends a query to the daemon over UDS, gets merged results from all provider TSDBs. Always fast, always indexed. Filter by `--provider` to query a single provider.

---

## Quick Start

```bash
# Install (macOS)
brew tap korjwl1/tap
brew install toki

# toki auto-detects ~/.claude and ~/.codex. No config needed.
# To manage providers manually: toki settings (TUI) or CLI:
# toki settings set providers --add claude_code
# toki settings set providers --add codex

# 1. Start the daemon (detaches to background by default)
toki daemon start

# 2. In another terminal — real-time event stream
toki trace

# 3. Reports (instant TSDB queries)
toki report                                   # all providers merged
toki report --provider claude_code            # single provider
toki report daily --since 20260301
toki report monthly

# 4. PromQL-style queries
toki report query 'usage{model="claude-opus-4-6"}[1h] by (model)'
toki report query 'sum(usage{since="20260301"}[1d]) by (project)'
toki report query 'events{since="20260320"}'
```

---

## Commands

### Daemon

```bash
toki daemon start       # Start (background)
toki daemon start --foreground  # Foreground mode (for debug)
toki daemon stop        # Stop
toki daemon restart     # Restart (reload settings)
toki daemon status      # Check status
toki daemon reset       # Wipe DB + reinitialize
```

### Report

```bash
# Summary
toki report                                         # all providers
toki report --provider claude_code                  # single provider
toki report --since 20260301 --until 20260331

# Time grouping
toki report daily --since 20260301
toki report weekly --since 20260301 --start-of-week tue
toki report monthly
toki report yearly
toki report hourly --since 20260301

# Session/project filters
toki report --group-by-session
toki report --project toki
toki report --session-id 4de9291e

# Provider filter
toki report --provider codex daily --since 20260301

# PromQL-style queries
toki report query 'usage{model="claude-opus-4-6"}[1h] by (model)'
toki report query 'sum(usage{since="20260301"}[1d]) by (project)'
toki report query 'events{since="20260320"}'
toki report query 'usage[1d] offset 7d'
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
| `--group-by-session` | Group by session (mutually exclusive with time subcommand) |
| `--session-id <PREFIX>` | Filter by session UUID prefix |
| `--project <NAME>` | Filter by project directory substring |
| `--provider <NAME>` | Filter by provider (`claude_code`, `codex`) |
| `--start-of-week mon\|tue\|...\|sun` | Only for `weekly` |

</details>

<details>
<summary>PromQL query syntax</summary>

```
[agg_func(] metric{filters}[bucket] [offset duration] [)] [by (dimensions)]
```

| Element | Description | Example |
|---------|-------------|---------|
| metric | `usage`, `sessions`, `projects`, `events` | `usage` |
| filters | `key="value"` pairs, comma-separated | `{model="claude-opus-4-6", since="20260301"}` |
| bucket | Time bucket (s/m/h/d/w) | `[1h]`, `[5m]`, `[1d]` |
| offset | Shift time window back | `offset 7d` |
| agg_func | `sum`, `avg`, `count` — collapse models | `sum(usage[1d])` |
| dimensions | Group by (model/session/project) | `by (model, session)` |

Filter keys: `model`, `session`, `project`, `provider`, `since`, `until`

</details>

### Trace

```bash
toki trace                                              # JSONL stream to stdout
toki trace --sink uds:///tmp/toki.sock                  # Relay to UDS
toki trace --sink http://localhost:8080/events           # Relay via HTTP
toki trace --sink print --sink http://localhost:8080     # Multi-sink
toki trace --no-cost                                    # Without cost field
```

### Settings

```bash
toki settings                                  # Open TUI (cursive)
toki settings set claude_code_root /path       # Set individual value
toki settings set providers --add claude_code  # Add a provider
toki settings set providers --add codex        # Add another provider
toki settings set providers --remove codex     # Remove a provider
toki settings get providers                     # List providers + status
toki settings get timezone                     # Get value
toki settings list                             # List all
```

<details>
<summary>Settings reference</summary>

| Setting | Description | Default |
|---------|-------------|---------|
| Providers | Enabled providers (`toki settings set providers --add/--remove`) | `[]` |
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

### Client Options

| Option | Applies to | Description |
|--------|-----------|-------------|
| `--output-format table\|json` | report | Override output format |
| `--sink <SPEC>` | trace | Output target: `print`, `uds://<path>`, `http://<url>` (repeatable) |
| `--timezone <IANA>` / `-z` | report | Override timezone |
| `--no-cost` | trace, report | Disable cost calculation |

> Trace always outputs JSONL. `--output-format` does not apply to trace. When using `--sink uds://` or `--sink http://`, spawn `toki trace` as a child process — it auto-terminates when the parent dies (SIGPIPE).

---

## Supported Providers

| Provider | CLI Tool | Data Format | Status |
|----------|---------|-------------|--------|
| `claude_code` | [Claude Code](https://claude.ai/code) | JSONL (append-only) | Supported |
| `codex` | [Codex CLI](https://github.com/openai/codex) | JSONL (append-only) | Supported |
| *(gemini)* | [Gemini CLI](https://github.com/google-gemini/gemini-cli) | JSON (full rewrite) | Planned |

Each provider gets its own isolated database (`~/.config/toki/<provider>.fjall`). Reports merge results across all enabled providers by default, or filter to a single provider with `--provider`.

---

## Documentation

| Document | Description |
|----------|-------------|
| **[Architecture & Design](docs/DESIGN.md)** | Daemon threads, TSDB schema, rollup strategy, checkpoint recovery, data flow |
| **[Usage Guide](docs/USAGE.md)** | Detailed command reference, output formats, library API, examples |
| **[JSONL Format Reference](docs/claude-code-jsonl-format.md)** | Claude Code JSONL structure, line types, parsing optimizations |
| **[Benchmark Details](benches/COMPARISON.md)** | Full comparison methodology, architecture analysis, scaling predictions |
| **[Codex CLI Analysis](docs/codex-cli-analysis.md)** | Codex CLI local data format, token structure, parsing strategy |
| **[Gemini CLI Analysis](docs/gemini-cli-analysis.md)** | Gemini CLI local data format analysis (future provider) |
| **[Why Not OpenTelemetry?](docs/why-not-otel.md)** | Why toki parses local files instead of receiving OTEL data |
| **[OTEL Comparison](docs/otel-comparison.md)** | OpenTelemetry implementation details: Claude Code vs Gemini CLI vs toki |

---

## Cost Calculation

All outputs include estimated cost (USD) per model, sourced from [LiteLLM](https://github.com/BerriAI/litellm) community pricing.

- **First run**: downloads LiteLLM JSON → filters by `litellm_provider` (Anthropic, OpenAI, Gemini) → caches to `~/.config/toki/pricing.json`
- **Subsequent runs**: HTTP ETag conditional request → 304 if unchanged (~50 ms, no body)
- **Offline**: uses cached data; if no cache, cost column is omitted
- **`--no-cost`**: skips price fetch entirely

---

## Privacy & Security

toki is designed to be **privacy-safe by architecture**, not by policy.

- **No prompt access**: toki's JSONL parser only deserializes the `"assistant"` type lines and extracts the `usage` object (token counts) and `model` field. User prompts, assistant responses, file contents, and thinking blocks are **never loaded into memory** — serde skips over them entirely without heap allocation.
- **No network transmission of your data**: all processing happens locally. toki never sends your data anywhere. The only outbound request is an optional pricing table fetch from the public LiteLLM repository (disable with `--no-cost`).
- **No logs of conversation content**: the TSDB stores only: timestamp, model name, session ID, source file path, and four token count integers. Nothing else.
- **Read-only access**: toki only reads session files. It never writes to, modifies, or deletes any CLI tool's data.

This is fundamentally different from OpenTelemetry-based monitoring, where the OTEL SDK runs inside the CLI process and may include prompts, tool calls, or API request bodies in log events depending on configuration. toki operates externally and sees only what it chooses to parse — which is strictly token metadata.

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
├── common/
│   ├── types.rs                    # Shared types & traits
│   └── time.rs                     # Fast timestamp parser (0.1µs)
├── daemon/                         # Daemon server components
│   ├── broadcast.rs                # BroadcastSink (zero-overhead fan-out)
│   ├── listener.rs                 # UDS accept loop + multi-DB query merge
│   └── pidfile.rs                  # PID file management
├── sink/                           # Output abstraction (Sink trait)
│   ├── print.rs                    # PrintSink (table/json → stdout)
│   ├── uds.rs                      # UdsSink (NDJSON → UDS)
│   └── http.rs                     # HttpSink (JSON POST)
├── providers/                      # Per-provider parsers (Provider trait)
│   ├── mod.rs                      # Provider trait, FileParser trait, registry
│   ├── claude_code/                # Claude Code JSONL parser
│   │   ├── mod.rs                  # ClaudeCodeProvider impl
│   │   └── parser.rs              # Session discovery + line parsing
│   └── codex/                      # Codex CLI JSONL parser
│       ├── mod.rs                  # CodexProvider impl
│       └── parser.rs              # Stateful parser (model tracking)
└── platform/macos/mod.rs           # macOS FSEvents watcher
```

---

## License

[FSL-1.1-Apache-2.0](LICENSE)
