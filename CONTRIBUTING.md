# Contributing to toki

Thanks for your interest in contributing. This guide is for anyone who wants to file a bug report, submit a code change, or improve the documentation.

The fastest path in:

- Bug reports and feature ideas → [open an issue](https://github.com/korjwl1/toki/issues) using the provided templates.
- Code or docs change → fork, branch, send a PR (see below).
- First-time contributor? Look for issues labeled `good first issue` on the issue tracker.

## Development setup

### Prerequisites

- Rust toolchain (latest stable). `Cargo.toml` does not pin an MSRV, so the latest stable from [rustup](https://rustup.rs/) is the safest choice. If you hit a build error on an older toolchain, upgrade before filing a bug.
- `cargo` (bundled with rustup).
- macOS, Linux, or Windows. macOS uses FSEvents, Linux uses inotify, Windows falls back to polling — all handled by the `notify` crate.

### Build and run

```bash
cargo build
cargo test

# Run daemon in foreground (for development)
cargo run -- daemon start --foreground
```

## Pull requests

1. Fork the repo and create a branch from `main`.
2. Make your changes.
3. Run `cargo test` and `cargo clippy`.
4. Open a PR with a clear description of what changed and why.

Keep PRs focused — one fix or feature per PR. Reviewers can ship a small, self-contained change faster than a large one.

### Commit messages

Existing history uses short, lowercase, type-prefixed subjects: `fix: ...`, `feat: ...`, `chore: ...`, `refactor: ...`, `docs: ...`. Follow that pattern so `git log --oneline` stays scannable.

## Issues

Found a bug or have a feature idea? Open an issue using the templates provided. Include the toki version, OS, and a minimal reproduction when reporting bugs — it saves a round of back-and-forth.

## Code style

- Run `cargo fmt` before committing. This keeps diffs free of formatting noise so reviewers can focus on intent.
- `cargo clippy` must pass with no warnings. Clippy catches a lot of subtle correctness issues; silencing it locally hides them from review.
- Match the structure of nearby files. Provider parsers live under `src/providers/<name>/`, sink implementations under `src/sink/`, and shared types under `src/common/`. New parsers should implement the `Provider` trait in `src/providers/mod.rs` rather than introducing a parallel abstraction.

## Tests

- Unit tests live next to the code they cover (`#[cfg(test)] mod tests`).
- Integration tests live in `tests/`.
- For parser changes, add a test with a representative JSONL line so future refactors do not silently break extraction.
