# Muxloom

**A native, SSH-first terminal multiplexer and control plane for AI agents.**

Muxloom keeps terminal processes alive after you disconnect, gives every agent a clear place in a keyboard-driven workspace, and collects the moments that need human attention. It is an original Linux multiplexer: no tmux runtime, browser, Electron shell, or network listener.

> Muxloom is pre-release software. `v0.1.0-alpha.1` is intended for testing alongside, not as an immediate replacement for, critical tmux sessions.

## Why Muxloom

- Native daemon-owned PTYs with detach and reattach.
- Responsive workspace rail and real horizontal or vertical pane layouts.
- Modal Vim-style navigation with direct terminal passthrough.
- Attention states for Codex, Claude, Hermes, and generic commands.
- Quick prompt composer, scrollback capture, usage, skills, config, schedules, and a terminal-native Hermes dashboard.
- Private Unix-socket IPC, no TCP listener, no telemetry, and no automatic permission approvals.

## Install from source

Linux, Rust 1.96 or newer, a C compiler, and `pkg-config` are required.

```bash
git clone https://github.com/Scriptception/Muxloom.git
cd Muxloom
cargo install --path . --locked
```

Prebuilt x86_64/aarch64 tarballs, `.deb`, and `.rpm` packages are planned for the first tagged alpha.

## Quick start

```bash
# Create a workspace and launch your login shell
muxloom new --name project

# Or launch an agent directly
muxloom new --name api --cwd ~/src/api -- codex
muxloom new --name frontend --cwd ~/src/web -- claude
muxloom new --name ops --cwd ~/src/infra -- hermes --tui

# Detach with q from navigation mode, then return later
muxloom attach api

# Scriptable inspection
muxloom list --json
muxloom doctor
```

`Ctrl-\` toggles between agent focus and navigation. In navigation mode:

| Key | Action |
| --- | --- |
| `j` / `k` | Select pane |
| `Enter` / `i` | Focus the selected terminal |
| `v` / `s` | Add a column/row shell split |
| `p` | Open the quick prompt composer |
| `[a` / `]a` | Previous/next attention item |
| `A` or `2` | Attention queue |
| `1`–`7` | Workspace, attention, usage, skills, config, schedules, Hermes |
| `?` | Keyboard guide |
| `q` | Detach without stopping processes |

## Agent hooks

Muxloom can install additive, non-blocking lifecycle hooks. It backs up existing JSON before writing and never returns an allow/deny decision.

```bash
muxloom setup hooks all --dry-run
muxloom setup hooks all
```

If a hook cannot reach Muxloom, it exits successfully within its short timeout so the agent continues unaffected. Inferred terminal-output states are visibly lower confidence than native hook events.

## Control surfaces

```bash
muxloom skills --json
muxloom config sources
muxloom config edit codex
muxloom schedule add --name morning-review --cron '0 0 9 * * Mon-Fri' -- codex
muxloom hermes status
muxloom hermes cron list
```

Token and cost fields are displayed only when a provider reports them. Muxloom does not fabricate progress percentages or cost estimates.

## Architecture

One binary contains a long-lived daemon and an attachable TUI client. The daemon owns PTYs, terminal output, workspace state, attention events, and schedules. Clients exchange typed, length-framed CBOR messages over a `0600` Unix socket under `$XDG_RUNTIME_DIR/muxloom/`.

Metadata is stored in SQLite under `$XDG_STATE_HOME/muxloom/`. Terminal contents remain in bounded memory and are not persisted to disk by default. See [Architecture](docs/ARCHITECTURE.md), [Security](SECURITY.md), and the [Roadmap](docs/ROADMAP.md).

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

Contributions are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT License](LICENSE-MIT), at your option.
