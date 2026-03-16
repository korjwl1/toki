# Claude Code JSONL Format Reference

Claude Code CLI records session logs as JSONL files under `~/.claude/projects/<encoded-path>/`.

## File Structure

```
~/.claude/projects/-Users-user-Documents-project/
├── 4de9291e-061e-414a-85cb-de615826aded.jsonl          # Parent session
├── 4de9291e-061e-414a-85cb-de615826aded/
│   └── subagents/
│       └── agent-aed1da92cc2e4e9e7.jsonl               # Subagent
└── db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b.jsonl          # Another session
```

- Parent session: UUID format (`8-4-4-4-12` hex) filename
- Subagent: `<UUID>/subagents/agent-*.jsonl`
- Subagent tokens are not included in the parent and are recorded in separate files

## Line Types (type field)

Each JSONL line is identified by its `"type"` field. 7 types observed:

| type | Purpose | Token Info | Size Characteristics |
|------|---------|-----------|---------------------|
| `assistant` | AI response (text, tool use) | **Present** (`message.usage`) | Avg ~1.5KB |
| `user` | User input | None | Avg ~8.3KB (large when file content included) |
| `progress` | Streaming progress | None (has nested assistant) | Avg ~1.1KB |
| `file-history-snapshot` | File snapshot | None | Avg ~0.6KB |
| `system` | System events (hooks, stop, etc.) | None | Avg ~0.6KB |
| `queue-operation` | Queue operation | None | Avg ~0.2KB |
| `pr-link` | PR link | None | ~0.2KB |

**Only the `assistant` type is relevant for token tracking.**

## assistant Line Detailed Structure

```json
{
  "parentUuid": "...",
  "isSidechain": false,
  "userType": "external",
  "cwd": "/path/to/project",
  "sessionId": "uuid",
  "version": "2.1.63",
  "gitBranch": "main",
  "message": {
    "model": "claude-opus-4-6",
    "id": "msg_01...",
    "type": "message",
    "role": "assistant",
    "content": [ ... ],
    "stop_reason": "end_turn",
    "stop_sequence": null,
    "usage": {
      "input_tokens": 3,
      "cache_creation_input_tokens": 5139,
      "cache_read_input_tokens": 9631,
      "output_tokens": 14,
      "server_tool_use": { ... },
      "service_tier": "...",
      "cache_creation": { ... },
      "inference_geo": "...",
      "iterations": 0,
      "speed": 0.0
    }
  },
  "requestId": "...",
  "type": "assistant",
  "uuid": "...",
  "timestamp": "2026-03-08T12:00:00Z"
}
```

### Fields Extracted by toki

| Field Path | Purpose |
|------------|---------|
| `type` | Identify `"assistant"` lines |
| `message.model` | Model name (aggregation key) |
| `message.id` | Event identifier |
| `message.usage.input_tokens` | Non-cached input tokens |
| `message.usage.cache_creation_input_tokens` | Cache creation input tokens |
| `message.usage.cache_read_input_tokens` | Cache read input tokens |
| `message.usage.output_tokens` | Output tokens |
| `timestamp` | Event time |

### Fields Ignored by toki

| Field Path | Reason |
|------------|--------|
| `message.content[]` | Text/thinking/tool_use content — not needed for token tracking, makes up bulk of each line |
| `message.usage.server_tool_use` | Server-side tool use metadata |
| `message.usage.service_tier` | Service tier |
| `message.usage.cache_creation` | Cache creation details |
| `message.usage.inference_geo` | Inference region |
| `message.usage.iterations` | Iteration count |
| `message.usage.speed` | Speed metric |
| `parentUuid`, `sessionId`, `cwd`, ... | Session metadata — currently unused |

### Content Block Types

3 types in the `message.content[]` array:

| content[].type | Description |
|----------------|-------------|
| `text` | Text response |
| `thinking` | Thought process (extended thinking) |
| `tool_use` | Tool invocation (file read, bash, search, etc.) |

## Parsing Optimizations

### Pre-filter

Lines not containing the `"assistant"` string are immediately skipped without JSON parsing.

- `user`, `file-history-snapshot`, `system`, `queue-operation`, `pr-link` → 100% skipped
- `progress` → has `"assistant"` nested inside `data.message`, passes pre-filter (false positive), rejected at serde stage where `type != "assistant"`

Measured on real data (5,162 lines, 13.2MB):
- **67% of data volume skipped without JSON parsing**
- Zero false negatives (no missed events)

### Targeted Struct Deserialization

Deserializes into a struct with only the needed fields instead of `serde_json::Value`.
Unnecessary fields like the `content` array are scanned by serde but not allocated on the heap.
Strings use `&str` borrowing to minimize copies.

## Caveats

- JSON key ordering is not guaranteed — optimizations depending on field position are risky
- The `"type"` field is located in the middle of the line (~280-388 bytes), not at the start
- The server outputs minified JSON but this may change — do not rely on whitespace presence
- `assistant` type lines have so far always included `message.usage` (zero cases without it)
