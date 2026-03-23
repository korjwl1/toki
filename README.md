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

## Table of Contents

- [Quick Start](#quick-start)
- [Who is this for?](#who-is-this-for)
- [How It Works](#how-it-works)
- [Performance](#performance)
- [Commands](#commands)
- [Supported Providers](#supported-providers)
- [Toki Monitor](#toki-monitor)
- [Planned Features](#planned-features)
- [Documentation](#documentation)
- [Cost Calculation](#cost-calculation)
- [Privacy & Security](#privacy--security)
- [Tech Stack](#tech-stack)
- [License](#license)

---

## Quick Start

```bash
# Install (macOS)
brew tap korjwl1/tap
brew install toki

# toki auto-detects ~/.claude and ~/.codex. No config needed.

# 1. Start the daemon
toki daemon start

# 2. Real-time event stream (in another terminal)
toki trace

# 3. Reports
toki report daily --since 20260301
toki report --provider claude_code
toki report monthly

# 4. PromQL-style queries
toki report query 'sum(usage{since="20260301"}[1d]) by (project)'
toki report query 'events{since="20260320"}'
```

---

## Who is this for?

- **Your terminal freezes on every token report?** toki is 14x faster on cold start and 1,700x faster on reports. Even 2 GB of data comes back in 7 ms.

- **Need more than "total tokens"?** Per-model, per-session, per-project, per-day breakdowns with PromQL-style queries. Filter by time range, group by any dimension, track costs — all in one command.

- **Don't want to set up OpenTelemetry?** No collector, no config files, no environment variables. Install toki, run it, done. It reads your existing session files directly — including months of history from before you installed it.

- **Using multiple AI CLI tools?** toki tracks Claude Code and Codex CLI in a single unified view. Filter by `--provider` when you need per-tool breakdown.

---

## How It Works

Docker-like daemon/client architecture:

```
toki daemon start     # always-on server   (≈ dockerd)
toki trace            # real-time stream    (≈ docker logs -f)
toki report           # instant TSDB query  (≈ docker ps)
```

- **daemon** — watches session logs from configured providers (Claude Code, Codex CLI), parses events, writes to per-provider embedded TSDBs (fjall). 4 base threads + 2 per connected trace client. Zero overhead when no trace clients are connected.
- **trace** — connects to the daemon over UDS for real-time JSONL event streaming. Supports multiple sinks (`--sink uds://`, `--sink http://`) for relaying to other services.
- **report** — sends a query to the daemon, gets merged results from all provider TSDBs. Always fast, always indexed. Filter by `--provider` to query a single provider.

---

## Performance

toki sits at 5 MB idle, near-zero CPU, and answers any report in 7 ms. Most alternatives re-read everything from scratch on every invocation — toki indexes once, then gets out of your way.

Benchmarked against [ccusage](https://github.com/ryoppippi/ccusage) (Node.js) and [zzusage](https://github.com/joelreymont/zzusage) (Zig) on the same dataset, disk cache purged before each run.

### Cold Start (full index build)

**14x faster** than ccusage, similar speed to zzusage but with **93% less memory**.

> In normal operation, toki resumes from its last checkpoint — only new data gets indexed.

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

> **Why does matching zzusage matter?** toki does strictly more work per line — TSDB writes, rollup aggregation, checkpoint persistence, and schema validation. zzusage skips all of this. Despite the extra workload, toki matches zzusage in wall-clock time.

</details>

### Report Speed (indexed TSDB query vs full re-scan)

**~7 ms** regardless of data size — **1,742x faster** than ccusage at 2 GB.

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

After cold start, toki drops to background-level resource usage.

| CPU | Memory | DB Size |
|-----|--------|---------|
| **~0%** | **5 MB** | **~3% of source data** (2 GB sessions → 64 MB TSDB) |

toki is the only tool here with a persistent idle state. The others pay full resource cost on every invocation.

> Measured on Apple M1 MacBook Air (8 GB RAM), macOS, power saving off.
> Reproduce: `sudo -v && python3 benches/benchmark.py run --purge --tool all`

---

## Commands

### Daemon

```bash
toki daemon start                # Start (background)
toki daemon start --foreground   # Foreground (for debug)
toki daemon stop                 # Stop
toki daemon restart              # Restart (reload settings)
toki daemon status               # Check status
toki daemon reset                # Wipe DB + reinitialize
```

### Report

```bash
# Summary
toki report
toki report --provider claude_code
toki report --since 20260301 --until 20260331

# Time grouping
toki report daily --since 20260301
toki report weekly --start-of-week tue
toki report monthly

# Session/project filters
toki report --group-by-session
toki report --project toki

# PromQL-style queries
toki report query 'sum(usage[1d]) by (project)'
toki report query 'events{since="20260320"}'
toki report query 'usage[1d] offset 7d'
```

For the full command reference, query syntax, and settings options, see the **[Usage Guide](docs/USAGE.md)**.

### Trace

```bash
toki trace                                          # JSONL stream to stdout
toki trace --sink uds:///tmp/toki.sock              # Relay to UDS
toki trace --sink http://localhost:8080/events       # Relay via HTTP
```

### Settings

```bash
toki settings                                  # Open TUI
toki settings set providers --add codex        # Add a provider
toki settings list                             # List all
```

---

## Supported Providers

| Provider | CLI Tool | Data Format | Status |
|----------|---------|-------------|--------|
| `claude_code` | [Claude Code](https://claude.ai/code) | JSONL (append-only) | Supported |
| `codex` | [Codex CLI](https://github.com/openai/codex) | JSONL (append-only) | Supported |
| *(gemini)* | [Gemini CLI](https://github.com/google-gemini/gemini-cli) | JSON (full rewrite) | Planned |

Each provider gets its own isolated database (`~/.config/toki/<provider>.fjall`). Reports merge results across all enabled providers by default, or filter to a single provider with `--provider`.

---

## Toki Monitor

[Toki Monitor](https://github.com/korjwl1/toki-monitor) is a macOS menu bar app built on top of toki. It gives you real-time token usage visualization without opening a terminal — animated rabbit character, sparkline graphs, dashboard with per-project charts, and velocity-based anomaly alerts. Installs the toki daemon automatically.

```bash
brew tap korjwl1/tap
brew install --cask toki-monitor
```

---

## Planned Features

| Feature | Description | Status |
|---------|-------------|--------|
| Gemini CLI | Google Gemini CLI provider support | Planned |
| `toki-sync` | Multi-device support — sync usage data across machines | Planned |

Have a feature request or found a bug? [Open an issue](https://github.com/korjwl1/toki/issues).

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

toki is privacy-safe by architecture, not by policy.

- **No prompt access**: the JSONL parser only deserializes token counts and model name from `"assistant"` lines. Prompts, responses, file contents, and thinking blocks are never loaded into memory — serde skips them without allocation.
- **No network transmission of your data**: all processing is local. The only outbound request is an optional pricing fetch from the public LiteLLM repo (`--no-cost` to disable).
- **No conversation logging**: the TSDB stores only timestamp, model name, session ID, source file path, project name, and token count integers.
- **Read-only access**: toki only reads session files. It never writes to or modifies any CLI tool's data.

---

## Tech Stack

| Purpose | Choice | Rationale |
|---------|--------|-----------|
| Database | fjall 3.x | Pure Rust LSM-tree, fits TSDB keyspace model |
| Concurrency | std::thread + crossbeam-channel | No async runtime conflicts, library-safe |
| Parallel scan | rayon | Cold start parallel file processing |
| File watching | notify 6.x | FSEvents (macOS), inotify (Linux), polling fallback per provider |
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
└── platform/mod.rs                 # FSEvents watcher + per-provider polling strategy
```

---

## License

[FSL-1.1-Apache-2.0](LICENSE)
