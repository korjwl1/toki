# Contributing to toki

Thanks for your interest in contributing!

## Development Setup

### Prerequisites

- Rust (latest stable)
- macOS, Linux, or Windows

### Build & Run

```bash
cargo build
cargo test

# Run daemon in foreground (for development)
cargo run -- daemon start --foreground
```

## Pull Requests

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Run `cargo test` and `cargo clippy`
4. Open a PR with a clear description of what and why

Keep PRs focused — one fix or feature per PR.

## Issues

Found a bug? Have a feature idea? Open an issue using the templates provided.

## Code Style

- `cargo fmt` before committing
- `cargo clippy` must pass with no warnings
- Follow existing patterns in the codebase
