# Muxloom Agent Guide

## Product invariants

- Muxloom is a native terminal multiplexer. Do not introduce a tmux runtime dependency or a web control plane.
- The daemon owns PTYs and durable metadata; the TUI is an attachable client.
- Agent permission decisions remain in the provider UI. Hooks report lifecycle state and must never auto-approve.
- Never invent token counts, costs, or determinate progress.
- Do not persist prompt or terminal content by default.

## Engineering standards

- Keep protocol and model types independent of Ratatui.
- Treat the versioned IPC schema as a public interface; reject oversized or incompatible frames.
- Pass command arguments as argv. Do not build shell strings from user input.
- Preserve unrelated user configuration, create backups before external config writes, and redact secrets.
- Use `cargo fmt`, Clippy with warnings denied, tests, and a release build before handoff.
- For TUI changes, verify 80x24, 120x40, and 200x60 layouts plus a real keyboard interaction.

## Installation and runtime verification

- Treat daemon restarts as destructive to live PTYs; enumerate affected panes and obtain confirmation first.
- Keep source, release build, installed binary, and running daemon distinct. Verify each before saying an update is installed.
- After installation, compare release and installed hashes, check `muxloom --version`, query `muxloom server status`, and run `muxloom list --json`.
- For protocol changes, test a stale daemon explicitly and require an actionable mismatch response rather than an unexplained EOF.
- Test terminal control keys as raw bytes as well as synthetic key events; Unix terminals may report `Ctrl-\\` as Control+`4`.

## Git

- Commit as `Scriptception <joshuarussell.online@gmail.com>`.
- Use `agent/<description>` branches and reviewed pull requests into `main`.
- Use Conventional Commit subjects and update `CHANGELOG.md` for user-visible changes.
