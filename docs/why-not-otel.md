# Why toki Parses Local Files Instead of Receiving OpenTelemetry

## Background

Claude Code, Codex CLI, and Gemini CLI all support OpenTelemetry (OTEL) export. A natural question arises: why not embed an OTLP receiver in toki and let CLI tools push data to it, instead of parsing their local session files?

This document explains the reasoning behind toki's file-based architecture.

## The Alternative: Embedded OTLP Receiver

```
CLI tool → OTLP export → toki (localhost OTLP server) → DB
```

Instead of:

```
CLI tool → local session files → toki (file watcher) → DB
```

## Why File Parsing Wins

### 1. Cold-start makes file parsers mandatory

toki's cold-start scans all historical session files and indexes them into the TSDB. This is required for retroactive analysis — seeing token usage from before toki was installed, or from periods when the daemon was stopped.

Since cold-start reads files, **file parsers must exist for every provider regardless**. If we added OTLP reception for watch mode, we would maintain two data ingestion paths:

- File parsers (all providers, for cold-start)
- OTLP event mappers (OTLP-supporting providers, for watch mode)

The file-only approach maintains one path:

- File parsers (all providers, for both cold-start and watch mode)

The same code handles both modes. One set of tests, one set of bugs, one thing to maintain.

### 2. Zero configuration

toki works by pointing at directories that already exist (`~/.claude`, `~/.codex`). No changes to any CLI tool's configuration are needed.

OTLP reception would require configuring each CLI tool to export to toki's endpoint:

- Claude Code: environment variables in `~/.claude/settings.json`
- Codex: `[otel]` section in `~/.codex/config.toml`
- Gemini: `telemetry` object in `~/.gemini/settings.json`

Even if toki auto-injected these settings, that's per-provider config injection logic to maintain — and it risks conflicting with users' existing OTEL setups.

### 3. Retroactive analysis

toki can analyze months of historical data the moment it's first installed. OTLP reception only captures data from the moment it starts running. The cold-start file scan is what makes retroactive analysis possible, and it uses the exact same parsers as watch mode.

### 4. No additional dependencies

File watching uses OS-native mechanisms (FSEvents, inotify) via the `notify` crate — already a dependency. OTLP reception would require embedding a gRPC or HTTP server stack (tonic/axum + prost for protobuf), significantly increasing binary size and dependency surface.

### 5. OTLP data still needs per-provider interpretation

Each CLI tool emits different OTLP signals with different metric names and structures:

| CLI | Token metric name | Signal type |
|-----|-------------------|-------------|
| Claude Code | `claude_code.token.usage` | Log Record |
| Gemini CLI | `gemini_cli.token.usage` | Counter |
| Codex | custom metrics | Log Record |

Receiving OTLP doesn't eliminate per-provider logic — it just moves it from "file schema parsing" to "OTLP event mapping." The complexity doesn't disappear; it changes form.

### 6. Data completeness

When toki's daemon restarts, file-based recovery is automatic — the checkpoint system resumes from the last processed line and catches up on everything written while toki was down.

With OTLP reception, events sent while toki is down are lost. The standard OTEL SDK's `BatchLogRecordProcessor` buffers in memory only — no disk-based retry for third-party endpoints. Cold-start would still recover the data from files, but this means the OTLP path adds no reliability value; it's purely redundant with the file path.

## When OTLP Would Make Sense

OTLP reception could be valuable if:

- toki dropped cold-start file parsing entirely (losing retroactive analysis)
- All target CLI tools supported OTLP (they don't — pi-agent, OpenCode have no or limited support)
- toki's goal were general-purpose observability rather than focused token tracking

None of these apply to toki's design goals.

## Summary

| Aspect | File parsing | OTLP reception |
|--------|-------------|----------------|
| Providers requiring parsers | All | All (cold-start still needs them) |
| Watch mode code paths | 1 (file) | 2 (file + OTLP) |
| User configuration needed | None | Per-CLI OTEL setup |
| Historical data | Immediate | From activation only |
| Binary dependencies | notify (existing) | + gRPC/HTTP stack |
| Daemon down recovery | Automatic (checkpoint) | Data lost until cold-start |

File parsing is the simpler, more complete, and more maintainable approach for toki's specific use case: **tracking token usage across multiple AI CLI tools with zero configuration.**
