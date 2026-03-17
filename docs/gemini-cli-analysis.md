# Gemini CLI Local Data Analysis

Analysis of how Gemini CLI stores conversation and token usage data locally, compared with Claude Code. Conducted for the purpose of adding Gemini support to toki.

## Data Directory Overview

| Item | Claude Code | Gemini CLI |
|------|------------|------------|
| **Root** | `~/.claude/` | `~/.gemini/` |
| **Session files** | `~/.claude/projects/<encoded-path>/<UUID>.jsonl` | `~/.gemini/tmp/<project-id>/chats/session-*.json` |
| **Subagents** | `~/.claude/projects/<path>/<UUID>/subagents/agent-*.jsonl` | N/A |
| **File format** | JSONL (one JSON object per line) | JSON (single file per session) |
| **Project ID** | Path encoded with `-` (e.g., `-Users-user-project`) | SHA-256 hash or human-readable slug |

## Gemini CLI Directory Structure

Top-level `~/.gemini/` contents:

| File/Directory | Purpose |
|---|---|
| `settings.json` | User configuration (MCP servers, auth, theme, session retention, etc.) |
| `projects.json` | Maps absolute project paths to human-readable project names |
| `google_accounts.json` | Active Google account info |
| `oauth_creds.json` | OAuth credentials (sensitive) |
| `installation_id` | UUID identifying this CLI installation |
| `GEMINI.md` | Global custom instructions (like Claude's `CLAUDE.md`) |
| `history/` | Per-project directory with `.project_root` mapping files |
| `tmp/` | Per-project session data, chat history, and logs |

## Storage Format Evolution

Gemini CLI's local storage has gone through three distinct generations.

### Generation 1: Hash + logs.json only (v0.1.x, ~Aug–Sep 2025)

```
~/.gemini/tmp/<SHA256-of-project-path>/
└── logs.json              ← user input only, no token data
```

- No `chats/` folder — full conversations and token usage were not persisted
- Project identified solely by SHA-256 hash of the absolute project path
- **Not useful for toki**: no token data available

### Generation 2: Hash + logs.json + chats/ (v0.3.0+, ~Oct 2025–Feb 2026)

```
~/.gemini/tmp/<SHA256-of-project-path>/
├── logs.json
└── chats/
    └── session-2026-01-20T01-54-4bddd070.json   ← contains token data
```

- `ChatRecordingService` introduced in v0.2.0 (Aug 2025), wired into chat flow in v0.3.0 (Sep 2025)
- Full session JSON files with messages, token usage, model info, tool calls
- Project path cannot be directly recovered from directory name (SHA-256 is one-way)
- Can attempt reverse mapping via `projects.json` (compute SHA-256 of known paths)

### Generation 3: Human-readable slug + .project_root (v0.29.0+, Feb 2026~)

```
~/.gemini/tmp/ddleague-clitrace/
├── .project_root          ← contains "/Volumes/SSD/Projects/Personal/ddleague-clitrace"
├── logs.json
└── chats/
    └── session-*.json
```

- Introduced in PR #17901 ("Shorten temp directory"), shipped in v0.29.0 (Feb 18, 2026)
- `ProjectRegistry` class assigns slugified directory names, handles collisions (e.g., `my-project-1`)
- `projects.json` stores the full path ↔ slug mapping
- `.project_root` file contains the normalized absolute path for ownership verification

## Version History

| Date | Version | Change |
|------|---------|--------|
| 2025.08 | v0.2.0 | `ChatRecordingService` introduced — `chats/` folder first appears |
| 2025.09 | v0.3.0 | Recording wired into chat flow — session JSON with token data starts |
| 2025.10 | v0.8.0 | `sessionRetention` setting added (opt-in, disabled by default) |
| 2026.02 | v0.29.0 | Hash→slug directory transition + `.project_root` + `projects.json`. 30-day retention enabled by default |
| 2026.03 | v0.33.0 | Retention warning removed, 30-day becomes silent default |

## Migration & Backward Compatibility

- `StorageMigration` class performs **copy-forward** from old hash directories to new slug directories on app startup
- Original hash directories are **not deleted** — both coexist
- Migration errors are swallowed gracefully (best-effort)
- This means **duplicate sessions** can exist across hash and slug folders for the same project

## Token Usage Format

### Claude Code (`assistant` type message)

```json
{
  "type": "assistant",
  "message": {
    "model": "claude-opus-4-6",
    "id": "msg_01...",
    "usage": {
      "input_tokens": 3,
      "cache_creation_input_tokens": 5139,
      "cache_read_input_tokens": 9631,
      "output_tokens": 14
    }
  },
  "timestamp": "2026-03-08T12:00:00Z"
}
```

4 token types: `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens`

### Gemini CLI (`gemini` type message)

```json
{
  "type": "gemini",
  "model": "gemini-3-flash-preview",
  "tokens": {
    "input": 6543,
    "output": 48,
    "cached": 0,
    "thoughts": 138,
    "tool": 0,
    "total": 6729
  },
  "timestamp": "2026-03-13T04:44:27.864Z"
}
```

6 token types: `input`, `output`, `cached`, `thoughts`, `tool`, `total`

### Token Type Mapping

| Gemini CLI | Claude Code Equivalent | Notes |
|-----------|----------------------|-------|
| `input` | `input_tokens` | Direct equivalent |
| `output` | `output_tokens` | Direct equivalent |
| `cached` | `cache_read_input_tokens` | Read from cache |
| `thoughts` | — | Chain-of-thought tokens (Gemini-specific) |
| `tool` | — | Tool-use tokens (Gemini-specific) |
| `total` | — | Sum field, can be computed |

Claude Code has `cache_creation_input_tokens` which Gemini does not track separately.

## Session File Structure (Gemini CLI)

### Session file naming convention

```
session-YYYY-MM-DDTHH-MM-<first-8-chars-of-sessionId>.json
```

### Session file top-level keys

```json
{
  "sessionId": "a075895e-5093-411a-b745-6ae9b837b41e",
  "projectHash": "<sha256>",
  "startTime": "2026-03-13T04:44:27.864Z",
  "lastUpdated": "2026-03-13T05:12:00.000Z",
  "kind": "main",
  "summary": "Auto-generated session summary",
  "messages": [...]
}
```

### Message types within a session

- **`info`**: system notifications (`id`, `timestamp`, `type`, `content`)
- **`user`**: user input (`id`, `timestamp`, `type`, `content` as array of `{text}`)
- **`gemini`**: model responses (`id`, `timestamp`, `type`, `content`, `thoughts`, `tokens`, `model`, `toolCalls`)

Only `gemini` type messages contain token usage data.

### logs.json (separate file, user input only)

```json
[
  {
    "sessionId": "a075895e-...",
    "messageId": 0,
    "type": "user",
    "message": "user input text",
    "timestamp": "2026-03-13T04:44:27.864Z"
  }
]
```

No token data — not useful for usage tracking.

## Documentation & Stability Assessment

- **No CHANGELOG.md** in the repository — release notes are auto-generated PR title lists
- **No stable contract** for local data storage format
- Storage format changes are buried among dozens of PRs in release notes
- **Still in 0.x versioning** — breaking changes expected
- The `tokens` object structure within session files has been stable since v0.3.0

## Implications for toki

### Discovery Pattern

```
~/.gemini/tmp/**/chats/session-*.json
```

Covers both Gen 2 (hash) and Gen 3 (slug) directories.

### Project Path Recovery Strategy

1. **Slug folder** (has `.project_root`): read the file directly
2. **Hash folder**: compute SHA-256 of paths listed in `projects.json` to reverse-map
3. **Unresolvable**: fall back to `unknown-<hash_prefix>` or `unknown-<slug>`

### Deduplication

Since migration copies (not moves) data, the same session may exist in both hash and slug directories. Deduplicate by `sessionId`.

### Parsing Strategy Differences

| Aspect | Claude Code (current) | Gemini CLI (new) |
|--------|----------------------|------------------|
| File format | JSONL → line-by-line streaming | JSON → full file parse |
| Incremental read | Append-only, read new lines from offset | Must re-parse entire file or detect by mtime/size |
| Pre-filter | Skip lines without `"assistant"` | Parse full JSON, filter `gemini` type messages |
| Checkpoint | Line-based xxHash3 | File-level (mtime + size or content hash) |

### Robustness

Given no stable contract and 0.x versioning, the Gemini parser should be lenient:
- Gracefully handle missing fields
- Skip unparseable session files with warnings
- Do not hard-fail on unexpected structure changes
