<p align="center">
  <img src="assets/logo.png" alt="toki logo" width="160" />
</p>

<h1 align="center">toki</h1>

<p align="center">
  <b>The smartest token usage tracker that stays out of your way.</b><br>
  Built with Rust | Daemon Architecture | 5MB Idle RAM | 7ms Reports | Zero Config
</p>

<p align="center">
  <sub><b>toki</b> = <b>to</b>ken <b>i</b>nspector — Fast and lightweight like a rabbit (tokki in Korean).</sub>
</p>

<p align="center">
  <a href="README.ko.md">🇰🇷 한국어</a>
</p>

---

### "Tools should feel like tools."

When using AI CLI tools, if your terminal freezes or you need complex server setups just to check your token usage, the tool is failing its purpose. **toki** is engineered to be so fast and lightweight that you won't even notice it's running, yet it provides powerful analytical capabilities when you need them.

---

## ✨ Key Differentiators

### 1. Zero Configuration
No need for complex OpenTelemetry collectors or environment variables. Just install and run; toki automatically discovers and analyzes logs left by Claude Code or Codex CLI.

### 2. Retroactive Analysis
While most trackers only record data from the moment they are installed, toki is different. It instantly indexes months of historical logs that existed before installation, giving you a complete picture of your usage.

### 3. Non-blocking Architecture
toki operates as a background daemon. It quietly watches your files while you work. It never interferes with your main process, ensuring zero impact on your CLI tool's performance.

### 4. Blazing Fast Reports
While other tools re-scan gigabytes of files every time you ask for a report, toki uses a dedicated Time-Series Database (TSDB). It can summarize gigabytes of data in just **7ms**.

---

## 🚀 Quick Start

### Installation (macOS)
Install easily via Homebrew:

```bash
brew tap korjwl1/tap
brew install toki
```

### Getting Started

toki auto-detects installed AI CLI tools. If `~/.claude` (Claude Code) or `~/.codex` (Codex CLI) exists, tracking starts immediately with zero configuration.

```bash
# Start the daemon (Indexing & Watching)
toki daemon start

# Check usage report
toki report

# Adjust settings via TUI if needed
toki settings
```

Running `toki settings` opens an interactive TUI where you can select providers, change paths, set timezone, and more.

---

## 📊 Performance & Benchmarks

toki isn't just fast; it's designed to use hardware resources effectively.

### Cold Start (Initial Indexing)
When run for the first time, toki utilizes `rayon`-based multi-threading to leverage all available CPU cores. While you might see high CPU usage initially, this is an **intentional design to process legacy data as quickly as possible**. This is a one-time cost; subsequent updates are incremental and nearly instant.

- **14x faster** than ccusage.
- **Memory Efficiency**: Uses **93% less memory** than zzusage (tested with 2GB dataset).

<p align="center">
  <img src="docs/bench_cold_start.png" alt="Cold Start Benchmark" width="800" />
</p>

### Report Speed
Once indexed, report performance is unrivaled. For a 2GB dataset, toki is over **1,700x faster** than ccusage.

| Dataset Size | toki (TSDB) | ccusage (Full Scan) | zzusage (Full Scan) |
|:---:|:---:|:---:|:---:|
| 100 MB | **0.007s** | 2.38s | 0.13s |
| 1 GB | **0.007s** | 10.88s | 0.76s |
| 2 GB | **0.007s** | 21.53s | 1.41s |

---

## 🛠 Technical Highlights

- **Non-blocking I/O**: Independent daemon structure ensures no impact on your workflow.
- **TSDB Powered**: Uses `fjall`, an embedded TSDB, to maximize query performance.
- **Smart Checkpoints**: `xxHash3`-based state tracking allows resuming exactly where it left off.
- **Resource Friendly**: Consumes only ~5MB of RAM while idling.

---

## 🔍 Why toki?

### vs OpenTelemetry (OTEL)
OTEL is a great standard but overkill for local CLI tools.
- **OTEL**: Requires a collector server, complex setup, no historical data support, network overhead.
- **toki**: No server, zero config, retroactive analysis, 100% local.

### vs ccusage / zzusage
These tools re-scan hundreds of JSONL files from scratch every time you request a report.
- **ccusage/zzusage**: High CPU/IO spike on every query, terminal freezing with large datasets.
- **toki**: Incremental background indexing, queries are instant database lookups.

---

## 📝 License
This project is licensed under FSL-1.1-Apache-2.0.

---
<p align="center">
  Built with 🦀 by <a href="https://github.com/korjwl1">korjwl1</a>
</p>
