# Codex CLI Local Data Analysis

Analysis of how Codex CLI stores conversation and token usage data locally, compared with Claude Code. Conducted for the purpose of adding Codex support to toki.

## Data Directory Overview

| Item | Claude Code | Codex CLI |
|------|------------|-----------|
| **Root** | `~/.claude/` | `~/.codex/` |
| **Session files** | `~/.claude/projects/<encoded-path>/<UUID>.jsonl` | `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<UUID>.jsonl` |
| **Subagents** | `~/.claude/projects/<path>/<UUID>/subagents/agent-*.jsonl` | N/A |
| **File format** | JSONL (one JSON object per line) | JSONL (one JSON object per line) |
| **Project ID** | Path encoded with `-` (e.g., `-Users-user-project`) | Session metadata `cwd` field |
| **History** | N/A | `~/.codex/history.jsonl` (user inputs only, not useful for token tracking) |

## Directory Structure

Top-level `~/.codex/` contents:

| File/Directory | Purpose |
|---|---|
| `config.toml` | User configuration (model, approval policy, OTEL, etc.) |
| `auth.json` | Authentication credentials (sensitive) |
| `history.jsonl` | User input history across sessions |
| `sessions/` | Per-date session JSONL files |
| `memories/` | Agent memory storage |
| `models_cache.json` | Cached model list |
| `version.json` | CLI version info |
| `logs_1.sqlite` | Internal logging database |
| `state_5.sqlite` | Internal state database |
| `log/` | Log files |
| `skills/` | Custom skills |
| `shell_snapshots/` | Shell state snapshots |
| `tmp/` | Temporary files |

## Session File Structure

### File naming convention

```
rollout-YYYY-MM-DDTHH-MM-SS-<session-UUID>.jsonl
```

Example:
```
~/.codex/sessions/2026/03/12/rollout-2026-03-12T00-35-10-019cdd89-9fd9-7f11-b555-459c0ec30834.jsonl
```

### Session file discovery pattern

```
~/.codex/sessions/**/*.jsonl
```

### Message types within a session

| Type | Wrapped in event_msg | Description |
|------|---------------------|-------------|
| `session_meta` | No | First line. Session ID, cwd, model_provider, CLI version, git info |
| `turn_context` | No | Turn metadata. Contains **model name**, approval policy, sandbox |
| `input_text` | No | User input |
| `token_count` | Yes (`event_msg`) | **Token usage data** — primary target for toki |
| `message` | Yes (`event_msg`) | Model response text |
| `response_item` | Yes (`event_msg`) | Response items |
| `function_call` | Yes (`event_msg`) | Tool invocation |
| `function_call_output` | Yes (`event_msg`) | Tool result |
| `reasoning` / `agent_reasoning` | Yes (`event_msg`) | Model reasoning |
| `task_started` / `task_complete` | Yes (`event_msg`) | Task lifecycle |
| `summary_text` | Yes (`event_msg`) | Summary |
| `workspace-write` | No | File write operations |
| `user_message` | No | User messages |

Only `token_count` (inside `event_msg`) contains token usage data.

### session_meta (first line)

```json
{
  "timestamp": "2026-03-11T15:35:35.678Z",
  "type": "session_meta",
  "payload": {
    "id": "019cdd89-9fd9-7f11-b555-459c0ec30834",
    "timestamp": "2026-03-11T15:35:10.066Z",
    "cwd": "/Users/korjwl1/Documents/ddleague/module/clitrace",
    "originator": "codex_cli_rs",
    "cli_version": "0.114.0",
    "source": "cli",
    "model_provider": "openai",
    "git": {
      "commit_hash": "9927396f...",
      "branch": "main",
      "repository_url": "https://github.com/..."
    }
  }
}
```

### turn_context (per-turn metadata, contains model name)

```json
{
  "timestamp": "2026-03-11T15:35:35.680Z",
  "type": "turn_context",
  "payload": {
    "turn_id": "019cdd8a-03b7-...",
    "cwd": "/Users/korjwl1/Documents/ddleague/module/clitrace",
    "current_date": "2026-03-12",
    "timezone": "Asia/Seoul",
    "approval_policy": "on-request",
    "model": "gpt-5.4",
    "personality": "pragmatic"
  }
}
```

## Token Usage Format

### Codex CLI (`token_count` inside `event_msg`)

```json
{
  "timestamp": "2026-03-11T15:36:16.626Z",
  "type": "event_msg",
  "payload": {
    "type": "token_count",
    "info": {
      "total_token_usage": {
        "input_tokens": 30395,
        "cached_input_tokens": 24192,
        "output_tokens": 165,
        "reasoning_output_tokens": 0,
        "total_tokens": 30560
      },
      "last_token_usage": {
        "input_tokens": 15262,
        "cached_input_tokens": 15104,
        "output_tokens": 82,
        "reasoning_output_tokens": 0,
        "total_tokens": 15344
      },
      "model_context_window": 258400
    },
    "rate_limits": {
      "limit_id": "codex",
      "limit_name": null,
      "primary": {
        "used_percent": 1.0,
        "window_minutes": 10080,
        "resets_at": 1773507709
      },
      "secondary": null,
      "credits": null,
      "plan_type": "free"
    }
  }
}
```

Two usage objects are provided:
- **`total_token_usage`**: Cumulative tokens for the entire session
- **`last_token_usage`**: Tokens for the most recent API call only — **use this for per-event tracking**

5 token types: `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens`, `total_tokens`

Note: The first `token_count` event in a session may have `"info": null` (no token data yet).

### Token Type Mapping

| Codex CLI | Claude Code Equivalent | Notes |
|-----------|----------------------|-------|
| `input_tokens` | `input_tokens` | Direct equivalent |
| `output_tokens` | `output_tokens` | Direct equivalent |
| `cached_input_tokens` | `cache_read_input_tokens` | Read from cache |
| `reasoning_output_tokens` | — | Codex-specific (o1/o3 reasoning tokens) |
| `total_tokens` | — | Sum field, can be computed |
| — | `cache_creation_input_tokens` | Claude-specific, Codex does not track |

### Model Name Discovery

Model names are **not in `token_count` events**. They appear in `turn_context` events. To associate a model with token usage:

1. Parse `turn_context` events to get the current model for each turn
2. Apply that model to subsequent `token_count` events until the next `turn_context`

Observed models: `gpt-5.4`, `gpt-5.3-codex`, `gpt-5.2-codex`

## Comparison with Claude Code

### Parsing Strategy

| Aspect | Claude Code | Codex CLI |
|--------|------------|-----------|
| File format | JSONL | JSONL |
| Append-only | Yes | Yes |
| Pre-filter keyword | `"assistant"` | `"token_count"` |
| Incremental read | Line-based xxHash3 checkpoint | **Same approach works** |
| Token data location | `message.usage` in assistant lines | `payload.info.last_token_usage` in event_msg lines |
| Model location | Same line (`message.model`) | Separate `turn_context` line (requires state tracking) |
| Session ID | Filename is UUID | `session_meta` payload `id` field (filename also contains UUID) |
| Project path | Parent directory name (encoded) | `session_meta` payload `cwd` field |
| Subagents | Separate files in `subagents/` directory | N/A |

### Key Differences for toki Implementation

1. **Model tracking requires state**: Unlike Claude Code where each assistant message contains its model, Codex requires tracking the most recent `turn_context` model and associating it with subsequent `token_count` events.

2. **Cumulative vs individual**: Codex provides both `total_token_usage` (cumulative) and `last_token_usage` (per-call). Use `last_token_usage` for individual event tracking, matching Claude Code's behavior.

3. **Project path from content, not directory**: Claude Code encodes the project path in the directory name. Codex stores sessions by date (`YYYY/MM/DD/`), so the project path must be read from `session_meta.payload.cwd`.

4. **Session ID extraction**: Can be extracted from filename (last segment of `rollout-{ts}-{UUID}.jsonl`) or from `session_meta` payload.

5. **Nested event structure**: Token data is wrapped in `event_msg` → `payload` → `info` → `last_token_usage`, requiring deeper JSON traversal than Claude Code's flat `message.usage`.

## history.jsonl (separate file, user input only)

```json
{"session_id":"019cdd89-...","ts":1773243335,"text":"user input text"}
```

No token data — not useful for usage tracking.

## Implications for toki

### Discovery Pattern

```
~/.codex/sessions/**/*.jsonl
```

### Parser Implementation

- Reuse existing `process_lines_streaming` and xxHash3 checkpoint system
- Pre-filter lines containing `"token_count"` for efficiency
- Extract `last_token_usage` from `payload.info`
- Track model from `turn_context` lines (requires minimal state: last seen model)
- Extract session ID and project path from `session_meta` (first line)

### Estimated Complexity

Low. The JSONL append-only format is identical to Claude Code, so the core infrastructure (checkpoint, incremental read, parallel cold start) can be shared directly. The main new work is:
- A new parser struct implementing `LogParser` / `LogParserWithTs` traits
- Model state tracking across `turn_context` → `token_count` events
- Token field mapping (5 Codex fields → common format)
