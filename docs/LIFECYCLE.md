# Daemon lifecycle

Detaching a client leaves the daemon and every PTY running. A clean `muxloom server stop --force` notifies attached clients, sends SIGTERM to live pane process groups, waits the configured grace interval, uses a final kill for stragglers, reaps children, records real exit timestamps/status where available, checkpoints SQLite, and removes the socket before the CLI reports success.

After a daemon crash, workspace names, pane commands, layouts, schedules, and attention events remain in SQLite. PTY file descriptors and their in-memory scrollback cannot be reconstructed. Panes without a recorded exit are reported with `state: "unknown"`, `daemon_lost: true`, and no fabricated `exited_at`. They are historical/orphan records and cannot be attached; kill the external process if it survived, prune the record, or create a new pane.

A host reboot has the same persistence boundary as a crash. The systemd user service can start the daemon. Workspaces created with `--respawn` explicitly opt into launching a fresh pane after daemon restart; all other commands remain historical and are never silently relaunched.
