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

Frames are capped at 8 MiB. The runtime directory and socket are private to the current user, and Linux peer credentials are checked before requests are accepted.

## Persistence

SQLite stores workspace, pane, and schedule metadata. PTY masters and terminal output exist in daemon memory; restarting the daemon marks historical panes exited because a lost PTY file descriptor cannot be safely reconstructed. This is deliberately honest rather than presenting dead processes as attached.

## Terminal rendering

`portable-pty` owns process terminals and `vt100` maintains client-side screen state. Ratatui provides responsive application chrome. This separates terminal correctness from the workspace and AI-control UX while keeping the application original and independent of tmux.

## Integrations

- Codex and Claude hooks are additive, non-blocking reporters.
- Hermes is invoked through documented local CLI operations rather than reading secret configuration or exposing its web dashboard.
- Generic commands receive process and inferred output state, clearly marked lower confidence.

Future adapter processes will use versioned NDJSON capability handshakes while the internal high-volume screen protocol remains CBOR.
