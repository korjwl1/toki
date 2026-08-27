<!--
AMENDMENT REPORT
- Version: 2.0.0
- Amended: 2026-08-27
- Reason for major bump: the v1 document described a Claude/macOS-only,
  rollup-on-write, print-only prototype. Production now supports Claude +
  Codex, macOS + Linux, daemon IPC, sync, PromQL queries, and non-rebuildable
  rate-limit window history. Those are principle-level changes.
-->

# Project constitution: toki

## Scope and current target

toki is a Rust library and CLI/daemon that reads local AI CLI session logs,
indexes token-usage events, reports usage/cost, tracks provider rate-limit
windows, and optionally syncs the resulting metadata between devices.

Current production scope:

- providers: Claude Code and Codex CLI;
- platforms: macOS and Linux;
- local daemon/client IPC over Unix domain sockets;
- provider-separated event stores and rate-limit window stores;
- local and toki-sync-backed query paths.

Gemini remains research/planned work. Windows is not supported while Unix
socket APIs are used unconditionally by the CLI, daemon, and UDS sink.

## Core principles

### 1. Library and CLI are both first-class

- `start(Config, Sink) -> Handle` and deterministic `Handle` shutdown remain
  usable by embedders.
- The CLI/daemon is a production interface, not a throwaway reference binary.
- Do not require an async runtime. The host may already own one; toki uses OS
  threads and channels to stay runtime-neutral.

### 2. Provider semantics stay separated

- Provider discovery, parsing, event identity, token schema, polling needs, and
  billing conventions live behind provider traits/modules.
- Do not force Claude and Codex token buckets into the same billing convention.
  Provider schemas decide which reported buckets are disjoint or overlapping.
- Shared query/wire types may normalize names, but provider-specific facts must
  not leak into generic parser/storage code.

### 3. Processing is incremental and recoverable

- After the required first import, normal processing MUST read only changed
  data. Full source rescans on every event/report are forbidden.
- Checkpoints use last-line length + xxHash3 rather than trusting byte offsets,
  so compaction and append recovery remain possible.
- Backpressure blocks producers instead of dropping token events/checkpoints.
- Dedup identity changes require adversarial tests covering snapshots, resumes,
  multiple events per message, and provider differences.

### 4. Concurrency is explicit and bounded

- Use `std::thread` and `crossbeam-channel`; no hidden async runtime.
- The worker→writer queue remains bounded. Per-provider writers serialize event
  mutation; optional poller/sync/backfill workers are lifecycle-owned by Handle.
- Thread count is configuration-dependent. Documentation and resource reviews
  must not assume a fixed four-thread daemon.
- Shutdown MUST stop network/background producers before writers and flush
  pending events, checkpoints, and open window observations.

### 5. Storage follows recovery properties

- Rebuildable events use per-provider `<provider>.fjall` databases with seven
  keyspaces: `checkpoints`, `meta`, `events`, `idx_sessions`, `idx_projects`,
  `dict`, and `idx_msg`.
- There is no rollup keyspace. Queries scan the time-ordered event store and use
  session/project indexes where applicable.
- Event schema incompatibility increments `SCHEMA_VERSION` and may reset only
  the rebuildable event DB.
- Rate-limit observations use separate `<provider>.windows.fjall` databases and
  `WINDOWS_SCHEMA_VERSION`. They MUST NOT be wiped by an event schema bump or
  `daemon reset`, because provider APIs/log retention may make them impossible
  to reconstruct.
- Unknown future window versions are preserved and not mutated by older builds.

### 6. Local and remote answers share contracts

- Query grammar, provider schema, cost precedence, timezone/week boundaries,
  window merge rules, and wire types should have a single source or parity test.
- Reversed ranges are errors, not successful empty reports.
- A date-only end includes the entire final day.
- Truncated or partial remote results must be surfaced; a limit is not a
  substitute for pagination or a correctness signal.
- Current remote limitations (pagination, RFC 3339 parity, deployment schema
  migration) remain explicit documentation until fixed.

### 7. Privacy claims enumerate every network path

- Session parsers deserialize/store token counts and routing metadata, never
  prompts, responses, file content, or thinking blocks.
- Local processing is the default, but documentation MUST list optional
  outbound paths: LiteLLM pricing, GitHub update checks, Claude window polling,
  and opt-in toki-sync.
- `--no-cost` only disables the relevant pricing work; it must not be described
  as a global offline switch.
- Plaintext sync (`--no-tls`) is development/LAN/VPN-only and must be labelled
  insecure.

### 8. Tests prove behavior, not implementation shape

- A regression test should fail against the pre-fix behavior whenever practical.
- Characterization tests are welcome but must not be reported as discriminating
  regression tests if old code already passed them.
- Parser changes include representative provider lines. Storage/sync races need
  concurrency or real-boundary tests, not only sequential handler tests.
- Local/remote parity changes test both paths and exact boundary events.

## Development workflow

1. Inspect existing code, docs, and compatibility constraints.
2. Write or update a behavior-level specification for user-visible changes.
3. Implement the smallest coherent change behind the existing architecture.
4. Add discriminating tests and relevant integration coverage.
5. Run formatting, tests, and Clippy; document any environmental skips.
6. Review compatibility, privacy/network effects, CPU/memory bounds, and release
   ordering before merge.

## Quality gates

Before merging:

- [ ] `cargo fmt --check`
- [ ] `cargo test`
- [ ] `cargo clippy --all-targets --all-features`
- [ ] New CLI examples agree with generated `--help`
- [ ] Event layout changes follow `SCHEMA_VERSION` rules
- [ ] Window layout changes preserve non-rebuildable history
- [ ] Protocol changes are tested across toki, toki_sync, protocol, and monitor
- [ ] New network activity and partial-result behavior are documented

## Release dependency gate

The development branch currently uses protocol types newer than the published
v1.0.0 tag and patches to `../toki_sync_protocol`. A release checkout cannot use
that sibling path. The mandatory order is:

1. tag toki-sync-protocol v1.1.0;
2. repin toki and toki_sync to v1.1.0;
3. remove both local patch sections;
4. verify clean standalone builds;
5. release the sync daemon before consumers that read its window data.

## Governance

- Version: 2.0.0
- Ratified: 2026-03-13
- Last amended: 2026-08-27
- Major: remove/redefine a principle; minor: add/expand a principle; patch:
  clarify wording without changing obligations.
