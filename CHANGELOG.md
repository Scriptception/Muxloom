# Changelog

All notable changes are documented here. Muxloom follows Semantic Versioning.

## [Unreleased]

## [1.0.0] - Unreleased

### Added

- Terminal-faithful cell rendering, cursor placement, mouse/paste/key passthrough, copy-mode scrolling, pane zoom, spatial layouts, exact PTY resizing, agent launchers, and configurable key bindings.
- Persisted layout trees and attention events, workspace lifecycle and attention CLI parity, shell completions, man-page generation, stable JSON documentation, and daemon lifecycle documentation.
- Protocol handshake/compatibility errors, chunked attach replay and lag resync, SQLite migrations/hardening, graceful shutdown, child reaping/exit status, schedule overlap protection, frame fuzzing, and release packaging with SBOMs/signatures/provenance.

### Changed

- Removed static Usage, Skills, Config, and Hermes views from primary navigation; provider data is never presented unless it is real and attributable.
- Hooks use connect-only delivery, truthful confidence, environment-bound pane identity, and current Codex/Claude lifecycle schemas.

## [0.1.0-alpha.1] - 2026-07-16

### Added

- Native daemon-owned Linux PTYs with detach, reattach, input, resize, capture, and bounded scrollback.
- Responsive Ratatui workspace with Vim navigation, pane splits, quick prompts, and attention queue.
- Versioned CBOR IPC over a private Unix socket and SQLite metadata.
- Codex and Claude lifecycle hook ingestion with additive safe setup.
- Usage, skills, config, native schedules, and terminal-native Hermes control surfaces.
- Linux CI, security checks, packaging metadata, documentation, and community health files.

[Unreleased]: https://github.com/Scriptception/Muxloom/compare/v0.1.0-alpha.1...HEAD
[1.0.0]: https://github.com/Scriptception/Muxloom/compare/v0.1.0-alpha.1...v1.0.0
[0.1.0-alpha.1]: https://github.com/Scriptception/Muxloom/releases/tag/v0.1.0-alpha.1
