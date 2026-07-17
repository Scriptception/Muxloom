# Security policy

## Supported versions

Until the first stable release, security updates target the latest tagged alpha.

## Reporting a vulnerability

Please use GitHub's private **Report a vulnerability** security-advisory flow for `Scriptception/Muxloom`. Do not open a public issue containing exploit details, credentials, terminal transcripts, or private configuration.

Include the affected version, Linux distribution, reproduction steps, expected impact, and any suggested mitigation. You should receive an acknowledgement within seven days.

## Security model

- Muxloom accepts local connections only through a mode-`0600` Unix socket and checks peer user identity.
- Hook input is length-limited and schema-decoded. A payload pane ID is ignored unless it agrees with the daemon-provided `MUXLOOM_PANE_ID`; hooks never make permission decisions.
- Commands are spawned from argv, not interpolated shell strings.
- Prompt and terminal data stay in bounded memory by default.
- Config writes reject symlinks, use private create-new temporary files, are validated, backed up, and scoped to the current user.
- Muxloom has no web server, remote API, telemetry, or automatic updater.

Running AI agents and shell commands remains equivalent to running them directly as your Unix user. Muxloom does not sandbox providers or override their permission systems.

## Threat model

The trust boundary is the local Unix account. The runtime directory and socket are private, every peer UID is checked, frames are capped at 8 MiB, and all commands remain argv arrays. Another process running as the same UID can read or manipulate the user's files and is therefore already inside this boundary. Remote terminal peers only reach Muxloom through the user's SSH session; Muxloom opens no network listener.

Terminal output and prompts may contain secrets. Output is bounded in memory and is not persisted. Prompt history is disabled by default and must be explicitly enabled. Crash metadata stores command argv, cwd, PID, state, and exit status, but never terminal contents.
