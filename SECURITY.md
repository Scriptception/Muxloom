# Security policy

## Supported versions

Until the first stable release, security updates target the latest tagged alpha.

## Reporting a vulnerability

Please use GitHub's private **Report a vulnerability** security-advisory flow for `Scriptception/Muxloom`. Do not open a public issue containing exploit details, credentials, terminal transcripts, or private configuration.

Include the affected version, Linux distribution, reproduction steps, expected impact, and any suggested mitigation. You should receive an acknowledgement within seven days.

## Security model

- Muxloom accepts local connections only through a mode-`0600` Unix socket and checks peer user identity.
- Hook input is length-limited and schema-decoded. Hooks never make permission decisions.
- Commands are spawned from argv, not interpolated shell strings.
- Prompt and terminal data stay in bounded memory by default.
- Config writes are previewable, validated, backed up, and scoped to the current user.
- Muxloom has no web server, remote API, telemetry, or automatic updater.

Running AI agents and shell commands remains equivalent to running them directly as your Unix user. Muxloom does not sandbox providers or override their permission systems.
