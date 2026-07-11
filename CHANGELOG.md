# Changelog

All notable user-facing changes to `decent` are recorded here. The node,
Tauri app, npm protocol package, and wire protocol have independent versions.
Protocol-package history lives in `packages/protocol/CHANGELOG.md`.

The format follows Keep a Changelog and semantic versioning.

## [0.0.6] - 2026-07-11

### Fixed

- **Token migration was dead** — `token_path()` checked the same path for both
  old and new (`~/.config/decent/` instead of `~/.config/decent-node/`), so the
  migration condition was always false. Fixed: old path is now correctly
  `~/.config/decent-node/worker-token`. The token copies on the first command
  run after upgrade (status, start, install — any command that reads the token).
- **Legacy daemon detection in `decent status`** — when the old
  `com.decent-render.decent-node` daemon is still running, status now shows a
  warning: "Run `decent install` to migrate the token + daemon label."
- **Legacy plist cleanup** — `decent install` now removes the old plist file
  after installing the new one, so the legacy daemon doesn't reload on next
  login.
- Migration prints a confirmation message: "Migrated token from
  ~/.config/decent-node/ → ~/.config/decent/"

### Migration from v0.0.5 (broken) or v0.0.4

The v0.0.5 release had the dead migration bug. If `decent status` reports no
token but the old daemon is still running, the fix is:

```bash
brew upgrade decent-render/tap/decent    # installs v0.0.6
decent status                             # auto-migrates token, shows legacy warning
decent install                            # migrates daemon: unloads old label, loads new
decent status                             # confirms: token=yes, daemon=running
```

The old daemon keeps running until `decent install` — no gap in render capacity.

## [0.0.5] - 2026-07-11

### Changed

- **CLI renamed from `decent-node` to `decent`.** The binary, crate, and all
  user-facing command references use `decent` now. A `decent-node` compatibility
  shim is published alongside `decent` — it prints a deprecation warning and
  forwards all arguments to `decent`. The shim will be removed in v0.1.
- **Config/log path migration:** `~/.config/decent-node/` → `~/.config/decent/`.
  The token file is auto-migrated on first run if the new path doesn't exist.
- **Launchd label migration:** `com.decent-render.decent-node` →
  `com.decent-render.decent`. The legacy agent is automatically unloaded during
  `decent install`; status checks recognize both labels during transition.
- Removed misleading post-install login tip (the token guard already catches
  missing tokens before reaching that line).

### Migration for existing v0.0.4 installs

```bash
brew upgrade decent-render/tap/decent    # installs the new `decent` binary
decent status                             # token auto-migrated, daemon label updated
```

The old `decent-node` command continues to work via the shim.

## [0.0.4] - 2026-07-10

### Added

- `pause` and `resume` controls for the installed launchd daemon.
- Live terminal dashboard via `decent tui`.
- Live daemon status snapshot with connection state, active job, progress, and
  session counters.
- Running `decent` without a subcommand now opens status.

### Changed

- Consolidated CLI helpers and improved TUI/operator copy.

### Release integrity

- v0.0.4 must ship through the complete cargo-dist pipeline and restore the
  installer/checksum/manifest asset contract after partial historical releases.
- The connection-state transition test now synchronizes on status events instead
  of racing the release suite against its heartbeat shutdown timer.

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
