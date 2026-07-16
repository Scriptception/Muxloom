# Contributing to Muxloom

Thank you for helping make multi-agent terminal work calmer and faster.

## Development setup

1. Install Rust 1.96 or newer, a C compiler, and `pkg-config`.
2. Fork and clone the repository.
3. Create a focused branch from `main`.
4. Run the full local checks before opening a pull request:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

Test daemon changes with a disposable workspace. Never point destructive experiments at irreplaceable terminal sessions.

## Pull requests

- Explain user impact and architectural trade-offs.
- Add tests for protocol, state-reducer, PTY, scheduler, or rendering behavior.
- Include terminal dimensions and interaction steps for TUI changes.
- Keep provider-specific parsing behind its integration boundary.
- Do not include secrets, transcripts, generated build output, or unrelated formatting.

By contributing, you agree that your contribution is licensed under `MIT OR Apache-2.0`.
