# Roadmap

## Implemented release line

- Terminal fidelity: exact PTY sizing, cell attributes/cursor, input/paste/mouse passthrough, bounded scrollback/copy mode, chunked replay, and lag resynchronization.
- Layout and navigation: persisted binary split trees and ratios, spatial focus, workspace number keys, pane zoom, collapsible chrome, configurable key bindings, and workspace lifecycle.
- Agent control: interactive persisted attention, truthful native/inferred confidence, multi-line targeted/broadcast prompts, and detected Codex/Claude/Hermes launchers. Surfaces without real live data were removed from primary navigation.
- Reliability: graceful signal/request shutdown, child reaping and exit status, honest crash/orphan metadata, opt-in respawn, schedule overlap protection/cleanup, SQLite migrations/hardening, mandatory protocol negotiation, and deny-by-default read-only clients.
- Production engineering: broader e2e/unit coverage, completions, man-page generation, stable JSON and lifecycle docs, frame fuzzing, distro smoke tests, and draft release artifacts with packages, checksums, SBOMs, signatures, and provenance.

## Later

- macOS evaluation and platform-specific PTY hardening.
- Encrypted, explicitly opt-in disk scrollback and searchable cross-session archives.
- Provider-supported structured usage adapters when stable attributable sources exist.
- Signed package repositories after standalone packages have field validation.

Muxloom remains terminal-first, local by default, and free of required web services.
