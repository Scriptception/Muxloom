# Muxloom

**A native, SSH-first terminal multiplexer and control plane for AI agents.**

Muxloom keeps terminal processes alive after you disconnect, gives every agent a clear place in a keyboard-driven workspace, and collects the moments that need human attention. It is an original Linux multiplexer: no tmux runtime, browser, Electron shell, or network listener.

> The `v1.0.0` source line implements the production milestone set. Until a signed release is published, build from source and validate it alongside—not as an immediate replacement for—critical multiplexer sessions.

## Why Muxloom

- Native daemon-owned PTYs with detach and reattach.
- Responsive workspace rail and real horizontal or vertical pane layouts.
- Modal Vim-style navigation with direct terminal passthrough.
- Attention states for Codex, Claude, Hermes, and generic commands.
- Scrollback/copy mode, a multi-line prompt composer, persisted layouts, schedules, and an interactive attention queue.
- Private Unix-socket IPC, no TCP listener, no telemetry, and no automatic permission approvals.

## Install from source

Linux, Rust 1.96 or newer, a C compiler, and `pkg-config` are required.

```bash
git clone https://github.com/Scriptception/Muxloom.git
cd Muxloom
cargo install --path . --locked
```

Tagged releases publish x86_64/aarch64 tarballs, `.deb`, and `.rpm` packages with checksums, CycloneDX SBOMs, Sigstore signatures, and build provenance. Package installation also provides the `muxloom.service` systemd user unit.

## Quick start

```bash
# Create a workspace and launch your login shell
muxloom new --name project

# Or launch an agent directly
muxloom new --name api --cwd ~/src/api -- codex
muxloom new --name frontend --cwd ~/src/web -- claude
muxloom new --name ops --cwd ~/src/infra -- hermes --tui

# Detach with d from navigation mode, then return later
muxloom attach api

# Scriptable inspection
muxloom list --json
muxloom doctor
```

`Ctrl-\` toggles between agent focus and navigation. `F12` is a fallback for terminals that reserve or remap control characters. In navigation mode:

| Key | Action |
| --- | --- |
| `h` / `j` / `k` / `l` | Select the spatially adjacent pane |
| `Shift-Tab` / `Tab`, `1`–`9` | Switch workspace |
| `c` | Create and switch to a named workspace |
| `Enter` / `i` | Focus the selected terminal |
| `v` / `s` | Choose a shell, detected agent, template, or custom command, then choose its cwd |
| `p` | Open the multi-line prompt composer (`Shift-Enter`; `Ctrl-b` broadcasts) |
| `PgUp` | Enter copy/scrollback mode (`PgDn`, `g`, `G`, `Esc`) |
| `z` / `b` / `o` | Zoom pane / toggle workspace rail / toggle context |
| `[a` / `]a` | Previous/next attention item |
| `A` | Attention queue (`j/k`, `Enter`, `r`, `d`, `R`) |
| `Space` then `a`/`s`/`?` | Attention / schedules / keyboard guide |
| `d` | Detach without stopping processes |

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
muxloom schedule disable SCHEDULE_ID
muxloom schedule enable SCHEDULE_ID
muxloom hermes status
muxloom hermes cron list
```

Provider token/cost UI is deliberately omitted until a provider offers a stable, attributable local data source. Muxloom does not fabricate progress percentages or cost estimates.

Launcher templates and repository discovery roots are configured in `config.toml`. Run `muxloom config path` to locate it; the defaults scan `~/src` to depth four and always include recent workspace directories.

## Architecture

One binary contains a long-lived daemon and an attachable TUI client. The daemon owns PTYs, terminal output, workspace state, attention events, and schedules. Clients exchange typed, length-framed CBOR messages over a `0600` Unix socket under `$XDG_RUNTIME_DIR/muxloom/`.

Metadata is stored in SQLite under `$XDG_STATE_HOME/muxloom/`. Terminal contents remain in bounded memory and are not persisted to disk by default. See [Architecture](docs/ARCHITECTURE.md), [Security](SECURITY.md), and the [Roadmap](docs/ROADMAP.md).

Layouts, attention events, schedules, and exit status survive a daemon restart. PTY streams do not: a daemon crash records a pane as `daemon_lost` instead of inventing an exit time. See [Daemon lifecycle](docs/LIFECYCLE.md) and the stable [`list --json` schema](docs/CLI_JSON.md).

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
