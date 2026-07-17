# Architecture

## Boundaries

Muxloom is one distributable binary with three runtime roles:

1. The CLI performs one-shot typed operations.
2. The daemon owns PTYs, workspaces, output buffers, agent state, schedules, and SQLite metadata.
3. The TUI attaches to a workspace and renders server snapshots and deltas.

The domain model and protocol do not depend on Ratatui. Provider hooks normalize into the shared `AgentState` and `AttentionEvent` model rather than leaking provider formats into the UI.

## Data flow

```text
PTY reader thread -> runtime channel -> bounded output + state reducer -> broadcast
CLI/TUI -> length-framed CBOR -> Unix socket -> typed daemon request -> PTY/provider
agent hook JSON -> muxloom hook -> normalized event -> attention queue
```

Every connection begins with a protocol-range handshake. Frames are capped at 8 MiB, replay is chunked to 256 KiB, and a lagging client receives a fresh per-pane snapshot rather than continuing from a corrupt byte gap. The runtime directory and socket are private to the current user, and effective-user peer credentials are checked before requests are accepted.

## Persistence

SQLite uses versioned migrations, WAL, foreign keys, a busy timeout, and transactions for related writes. It stores workspace/layout, pane/exit, attention, and schedule metadata. PTY masters and terminal output exist in daemon memory. After a crash, a matching surviving PID is reported as an output-less orphan with `daemon_lost`; Muxloom never fabricates an exit timestamp. Opt-in respawn workspaces create a fresh PTY and pane record.

## Terminal rendering

`portable-pty` owns process terminals and `vt100` maintains client-side screen state. The renderer maps the persisted binary split tree to pane rectangles, uses each inner rectangle for both PTY and parser sizing, and copies cell colors/attributes/cursor directly into Ratatui's buffer. Ratatui's diffing plus a 16 ms redraw coalescer avoids repainting unchanged cells.

## Integrations

- Codex and Claude hooks use their current lifecycle schemas and are additive, connect-only, non-blocking reporters.
- Hermes is invoked through documented local CLI operations rather than reading secret configuration or exposing its web dashboard.
- Generic commands receive process and inferred output state, clearly marked lower confidence.

Future adapter processes may use versioned NDJSON capability handshakes while the internal high-volume screen protocol remains CBOR.
