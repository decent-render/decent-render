# Changelog

All notable user-facing changes to `decent-node` are recorded here. The node,
Tauri app, npm protocol package, and wire protocol have independent versions.
Protocol-package history lives in `packages/protocol/CHANGELOG.md`.

The format follows Keep a Changelog and semantic versioning.

## [Unreleased]

### Added

- `pause` and `resume` controls for the installed launchd daemon.
- Live terminal dashboard via `decent-node tui`.
- Live daemon status snapshot with connection state, active job, progress, and
  session counters.
- Running `decent-node` without a subcommand now opens status.

### Changed

- Consolidated CLI helpers and improved TUI/operator copy.

### Release integrity

- v0.0.4 must ship through the complete cargo-dist pipeline and restore the
  installer/checksum/manifest asset contract after partial historical releases.

## [0.0.3] - 2026-07-09

### Added

- Token login/logout, install guard, status, upgrade, and real version reporting.

### Known release issue

- Published manually with only the Apple Silicon archive after the cargo-dist
  plan job was not acquired. Installer, checksums, and dist manifest are absent.

## [0.0.2] - 2026-07-09

### Known release issue

- Tag exists, but no GitHub Release was produced because the macOS build job was
  not acquired.

## [0.0.1] - 2026-07-08

### Added

- First cargo-dist release of the Apple Silicon node CLI.
- Homebrew installation path and protocol compatibility guard.
