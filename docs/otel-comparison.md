# OpenTelemetry integration: Claude Code vs Gemini CLI

Comparison of how Claude Code and Gemini CLI implement OpenTelemetry, and how toki's local file parsing approach differs.

## Architecture overview

### Claude Code OTEL

Two independent export paths coexist within the Claude Code process:

```text
┌─ Claude Code Process ───────────────────────────────┐
│                                                      │
│  Event occurs (token usage, API call, etc.)          │
│       │                                              │
│       ▼                                              │
│  OTEL Logger ("com.anthropic.claude_code.events")    │
│       │                                              │
│       ├──→ [Path A] 3P OTLP Export (user-configured) │
│       │    BatchLogRecordProcessor                   │
│       │    scheduledDelay: 5s                         │
│       │    → OTLP Collector (http/protobuf|grpc)     │
│       │                                              │
│       └──→ [Path B] 1P Anthropic Internal Telemetry  │
│            BatchLogRecordProcessor                   │
│            scheduledDelay: 10s                        │
│            maxBatchSize: 200                          │
│            maxQueueSize: 8192                         │
│            → Anthropic server (HTTP POST JSON)       │
│                                                      │
│  Separate: PeriodicExportingMetricReader (5min)      │
│            → internal metrics (enterprise/team)      │
└──────────────────────────────────────────────────────┘
```

Key finding: Token usage is **not** recorded via OTEL Metrics API counters. Instead, individual events are emitted as **OTEL Log Records** and batch-exported. This means the data is not pre-aggregated — each token usage event is sent individually.

### Gemini CLI OTEL

```text
┌─ Gemini CLI Process ────────────────────────────────┐
│                                                      │
│  Event occurs                                        │
│       │                                              │
│       ├──→ Traces (BatchSpanProcessor)               │
│       │    → spans for LLM calls, tool calls, agents │
│       │                                              │
│       ├──→ Logs (BatchLogRecordProcessor)            │
│       │    → 40+ event types                         │
│       │                                              │
│       └──→ Metrics (PeriodicExportingMetricReader)   │
│            → 40+ counters & histograms               │
│                                                      │
│  Export targets (configurable):                      │
│    1. GCP Cloud Trace/Monitoring (native)            │
│    2. OTLP Collector (gRPC + GZIP / HTTP)            │
│    3. Local file                                     │
│    4. Console                                        │
└──────────────────────────────────────────────────────┘
```

## Comparison table

| Aspect | Claude Code | Gemini CLI |
|--------|------------|------------|
| **Signal types** | Logs (primary) + Metrics (enterprise) | Metrics + Logs + Traces |
| **Token data transport** | OTEL Log Records (individual events) | OTEL Counters (pre-aggregated) + Log events |
| **Metric count** | ~8 core metrics | 40+ metrics |
| **Configuration** | Environment variables only (`OTEL_*`) | Env vars + `settings.json` + CLI args |
| **GCP native** | No | Yes (Cloud Trace, Monitoring) |
| **SDK** | `@opentelemetry/sdk-node` | `@opentelemetry/sdk-node` |
| **Service name** | `claude-code` | `gemini-cli` |
| **Meter name** | `com.anthropic.claude_code` | `gemini-cli` |

## Claude Code OTEL details

### Path A: user-configured 3P OTLP export

Activated when `CLAUDE_CODE_ENABLE_TELEMETRY=1` is set.

| Setting | Default | Env Var |
|---------|---------|---------|
| Log batch delay | 5s | `OTEL_LOGS_EXPORT_INTERVAL` |
| Trace batch delay | 5s | — |
| Metric export interval | 60s | `OTEL_METRIC_EXPORT_INTERVAL` |
| Protocol | http/protobuf | `OTEL_EXPORTER_OTLP_PROTOCOL` |
| Supported protocols | http/protobuf, http/json, gRPC | — |
| Temporality (Prometheus) | CUMULATIVE | — |
| Temporality (OTLP) | SDK default (DELTA for sums) | — |
| Shutdown timeout | 2s | `CLAUDE_CODE_OTEL_SHUTDOWN_TIMEOUT_MS` |
| Flush timeout | 5s | `CLAUDE_CODE_OTEL_FLUSH_TIMEOUT_MS` |

### Path B: Anthropic 1P internal telemetry

Always active, independent of user settings.

| Setting | Value |
|---------|-------|
| Batch delay | 10s (adjustable via GrowthBook) |
| Max batch size | 200 |
| Max queue size | 8,192 |
| Endpoint | Anthropic internal server |
| Killswitch | `tengu_frond_boric.firstParty` (GrowthBook feature flag) |

### Metrics emitted

| Metric | Description | Unit |
|--------|-------------|------|
| `claude_code.token.usage` | Token consumption | tokens |
| `claude_code.cost.usage` | Session cost | USD |
| `claude_code.session.count` | Sessions started | count |
| `claude_code.lines_of_code.count` | Lines modified (added/removed) | count |
| `claude_code.commit.count` | Git commits created | count |
| `claude_code.pull_request.count` | Pull requests created | count |
| `claude_code.active_time.total` | Active usage time | seconds |
| `claude_code.code_edit_tool.decision` | Code edit tool decisions | count |

### Log events emitted

- `claude_code.user_prompt` — user submits a prompt
- `claude_code.tool_result` — tool completes execution
- `claude_code.api_request` — API request to Claude
- `claude_code.api_error` — API request fails
- `claude_code.tool_decision` — tool permission decision

### Retry/queue logic (1P path)

Failed exports are persisted to disk and retried with exponential backoff:

```text
Export failure
  → queueFailedEvents()
     → saves to: 1p_failed_events.{sessionId}.{uuid}.json
  → scheduleBackoffRetry()
     → delay = baseDelay × attempts² (exponential backoff)
  → On next session start: retryPreviousBatches()
     → reads previous session's failed files and re-sends
```

### Privacy controls

| Control | Default | Env Var |
|---------|---------|---------|
| Log user prompts | Disabled | `OTEL_LOG_USER_PROMPTS=1` |
| Log tool details | Disabled | `OTEL_LOG_TOOL_DETAILS=1` |
| Include session ID | Enabled | `OTEL_METRICS_INCLUDE_SESSION_ID` |
| Include version | Disabled | `OTEL_METRICS_INCLUDE_VERSION` |
| Include account UUID | Enabled | `OTEL_METRICS_INCLUDE_ACCOUNT_UUID` |

## Gemini CLI OTEL details

### Configuration

3-tier priority: CLI args > Environment variables > settings.json

```json
{
  "telemetry": {
    "enabled": true,
    "otlpEndpoint": "https://collector.example.com",
    "otlpProtocol": "grpc"
  }
}
```

| Setting | Env Var | Values |
|---------|---------|--------|
| Enabled | `GEMINI_TELEMETRY_ENABLED` | true/1, false |
| Target | `GEMINI_TELEMETRY_TARGET` | "local", "gcp" |
| OTLP Endpoint | `GEMINI_TELEMETRY_OTLP_ENDPOINT` | URL |
| OTLP Protocol | `GEMINI_TELEMETRY_OTLP_PROTOCOL` | "grpc", "http" |
| Log Prompts | `GEMINI_TELEMETRY_LOG_PROMPTS` | true/1, false |
| Output File | `GEMINI_TELEMETRY_OUTFILE` | file path |

### Export intervals

| Target | Interval |
|--------|----------|
| OTLP / File / Console | 10s |
| GCP Direct | 30s |

### Metrics (subset)

| Metric | Description |
|--------|-------------|
| `gemini_cli.token.usage` | Tokens by model and type |
| `gemini_cli.api.request.count` | API requests by model, status |
| `gemini_cli.api.request.latency` | API latency (histogram) |
| `gemini_cli.tool.call.count` | Tool call count |
| `gemini_cli.tool.call.latency` | Tool execution time |
| `gemini_cli.agent.run.count` | Agent execution count |
| `gemini_cli.agent.duration` | Agent run duration |
| `gemini_cli.memory.usage` | Memory usage |
| `gemini_cli.cpu.usage` | CPU utilization |
| `gemini_cli.performance.score` | Performance score (0-100) |

### Traces

Spans created for LLM calls, tool executions, and agent runs. Follows OpenTelemetry GenAI semantic conventions. Attributes include operation name, model, conversation ID, input/output messages.

## toki vs OTEL: token usage tracking

For the specific purpose of token usage monitoring:

| Aspect | OTEL (Claude Code) | toki |
|--------|-------------------|------|
| **Data source** | Same (assistant message tokens) | Same |
| **Data granularity** | Individual events (Log Records) | Individual events (JSONL lines) |
| **Transport** | Network (OTLP over HTTP/gRPC) | Local file I/O |
| **Latency** | 5-10s batch delay | Filesystem-event driven; macOS Codex has a 1s polling fallback |
| **Infrastructure** | Collector + backend required | None (standalone binary) |
| **Failure recovery** | File queue + exponential backoff | Checkpoint-based resume |
| **Subagent separation** | Aggregated (no separation) | Per-file separation |
| **Historical data** | From export activation onwards | All existing JSONL files |
| **Offline support** | No (network required) | Yes |
| **Claude Code overhead** | SDK memory + serialization + network I/O | Zero (reads existing files) |

### Data only available via OTEL

- `lines_of_code.count` (added/removed)
- `commit.count`, `pull_request.count`
- `code_edit_tool.decision`
- `tool_result`, `api_error` log events

### Data only available via toki

- Per-subagent token breakdown
- Message-level `message_id` tracking
- Historical data retroactive analysis
- Works in airgapped/offline environments
