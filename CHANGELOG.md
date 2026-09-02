# Changelog

All notable user-facing changes to `decent` are recorded here. The node,
Tauri app, npm protocol package, and wire protocol have independent versions.
Protocol-package history lives in `packages/protocol/CHANGELOG.md`.

The format follows Keep a Changelog and semantic versioning.

## [Unreleased]

### Fixed

- **`decent upgrade` claimed success on a no-op and restarted the daemon
  for nothing.** It ran `brew upgrade decent` without refreshing the tap, so
  on a node where brew's 24-hour auto-update had not fired it saw the old
  formula, printed "Upgraded decent via Homebrew" and kicked the daemon
  (which would have killed an in-flight render) while the binary stayed on
  the old version. It now pulls the `decent-render/tap` checkout first,
  compares the formula's version with the running one, and treats only a
  CHANGED on-disk binary as success: "Already on X — nothing to upgrade"
  (no restart) when current, and an error naming the channel version and
  the installed one when brew delivered nothing.

## [0.0.10] - 2026-09-02

Operator tooling, a locked-down runner environment, and owner-only files.

### Added

- **`decent doctor`** — one-screen health check: token shape, expiry and
  file modes, daemon, status freshness, dispatch reachability, free disk on
  the worker root, version. Exits 1 if any check FAILED.
- **`decent logs`** — the daemon log, last 50 lines by default (`-n`),
  `-f` to follow.
- `decent status` prints the log path and a `decent doctor` hint when
  something needs attention; the TUI footer shows the update sentence.

### Changed

- **The render runner child starts from an allowlisted environment**
  (`PATH`, `HOME`, `TMPDIR`, locale, TLS/proxy variables and the
  `DECENT_`/`REMOTION_`/`CHROME_`/`PUPPETEER_` prefixes) instead of
  inheriting the daemon's — a tenant bundle cannot read whatever else was
  in the operator's session.
- Job working directories are created exclusively with mode 0700; the
  worker root and the daemon log are owner-only (`decent install` tightens
  an existing 0644 log).
- The daemon log file no longer contains ANSI escape codes.
- The Tauri console defaults to the production `wss://` dispatch URL and
  refuses malformed tokens and non-TLS URLs off localhost, using the same
  checks as the CLI.

### Fixed

- `decent status` never said "up to date": it compared the snapshot's
  connection state to the wrong word, so a healthy node printed "unknown
  (daemon not connected)".


## [0.0.9] - 2026-08-26

The first-run release: a fresh `brew install` now reaches the real farm,
refuses to leak your token, and tells you the truth about what it knows.

### Fixed

- **`decent start` and `decent tui` defaulted to `ws://localhost:8790/ws`**
  while `decent install` defaulted to the production dispatch. Following the
  documented command retried 15× against nothing and exited 1. All three now
  default to the production URL; `--dispatch-url` / `DISPATCH_URL` override.
- **A remote `ws://` URL shipped the worker token in CLEARTEXT.** Non-`wss://`
  URLs to a non-local host are now refused, with a message saying why.
  Localhost stays plaintext for local development.
- **A 401 from dispatch was reported as "Dispatch unreachable"**, sending
  operators to debug their network when their token was invalid or revoked.
  Auth failure is now named as such and fails fast in under a second instead
  of climbing a retry ladder a bad credential can never satisfy.
- **`decent status` printed `update: up to date` when it had no snapshot at
  all.** It now reports an honest unknown.
- **The worker token was written 0644 and then chmod'd to 0600**, leaving a
  window where the fleet credential was world-readable; the migration from
  `~/.config/decent-node/` never chmod'd at all. The file is now created 0600.
- **`decent install` opened with `Load failed: 5: Input/output error`** on
  every first install — a best-effort unload of a unit that did not exist yet.
  It is only attempted when a plist is actually present.
- **`decent login` did not take effect while the daemon was running.**
- **The launchd legacy label check could never match** (`LEGACY_LABEL`
  contains `LABEL` as a substring), so `decent pause` contradicted
  `decent status` on a legacy-only install. Service units are also XML-escaped
  now, so a dispatch URL containing `&` no longer writes an invalid plist.

### Security

- **Artifact downloads are bounded and streamed.** They previously had no
  connect, read, or total timeout and buffered the entire artifact in memory
  (~340 MB for a browser) before the sha check. They are now timed, streamed
  to disk while hashing, size-capped, and cancellable. The sha is still
  verified before anything is extracted or executed.
- **Tar extraction is size-capped** on both the Rust and TypeScript paths, and
  traversal containment (`..` members, absolute paths, symlink write-through)
  is now pinned by tests against both bsdtar and GNU tar.

### Changed

- **A failed workdir purge is no longer silent.** It is retried, recorded, and
  surfaced to the operator — the purge is the property this crate is public to
  prove, and its failure mode must be visible.
- No exit path can skip the in-flight drain, and every in-progress teardown is
  awaited rather than only the most recent one. A draining node no longer
  advertises itself as idle.
- The supervisor echoes the optional assignment-attempt lease on accepted,
  progress, complete, and failed messages, so lifecycle-aware dispatch can
  reject delayed messages from an older retry. Protocol-v2 compatible with
  attempt-less assignments.
- `DECENT_MAX_CONCURRENT_JOBS` is removed. It was inert: the ceiling was
  ignored and the connection loop only ever holds one in-flight slot.

## [0.0.8] - 2026-08-26

Recorded retroactively — 0.0.8 shipped without a changelog entry.

### Fixed

- Reconnect after abrupt TLS closes instead of exiting, with jittered
  exponential backoff and a healthy-session reset.
- Size-capped LRU sweep over the node caches, ending the ~1 GB-per-job growth.
- `max_concurrent_jobs` is declared honestly on the wire.
- An idle-sleep assertion is held for exactly the job's lifetime.
- Honest operator surfaces across the CLI, TUI, and Tauri app.

### Changed

- Release builds use `precise-builds`, keeping the Tauri app's GTK stack out
  of the Linux builders.

## [0.0.7] - 2026-07-11

### Changed

- `decent login` now defaults to `https://decent-render.farm/devices` for
  pairing (was driffs `/settings/devices`). The `--app-url` flag overrides.
- Removed hardcoded `tenant: "driffs"` from the register message. The farm
  dispatch identifies workers by token (account → workspace), not tenant.
- Added `MIN_DISPATCH_VERSION` constant for future version-compat guard.

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
