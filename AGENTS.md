# Decent Render contributor guide

## Purpose and boundaries

This public Apache-2.0 repository contains the operator-side software and the
open wire contract for the Decent render network.

- `crates/supervisor-core/` — protocol types, connection/job orchestration,
  observability, and the purge invariant.
- `bins/decent/` — the primary operator CLI/TUI and launchd integration.
- `apps/decent-app/` — maintained secondary Tauri window over the same core.
- `packages/protocol/` — published TypeScript consumer surface and shared v2
  fixtures.

The dispatch service, tenant billing/credit ledger, and versioned render payloads
are private components in the separate `driffs` repository. Do not copy their
secrets, schemas, business logic, or payload source here.

## Non-negotiable invariants

1. **Purge is structural.** Every assigned job requires `purgeAfter: true` and
   every terminal path removes the per-job work directory. Preserve the
   `WorkDir::Drop` backstop and test failure/cancel paths.
2. **Real jobs default off.** A node must not accept real work unless the
   operator explicitly enables it. UI and CLI are controls; supervisor-core
   enforces the gate.
3. **Identity comes from signed credentials.** Never trust operator, tenant,
   device, or platform identity supplied as advisory registration data.
4. **Protocol changes are cross-language changes.** The Rust types, TS schemas,
   and `packages/protocol/fixtures/v2.json` must move together. Both conformance
   suites must pass before merge.
5. **The node stays payload-agnostic.** It verifies, launches, observes, cancels,
   uploads, and purges versioned payloads; private render implementation does not
   move into this repository.
6. **One core.** CLI, TUI, and Tauri must drive `supervisor-core`; do not create a
   second connection/job engine in a surface.

## Protocol source-of-truth wording

- Rust typed emitter/consumer: `crates/supervisor-core/src/protocol.rs`
- TypeScript consumer API: `packages/protocol/src/index.ts`
- Shared wire truth: `packages/protocol/fixtures/v2.json`

Rust generates/locks the fixtures; Rust and TypeScript both round-trip the same
fixtures. No side may change the wire contract alone.

## Required gates

Run from the repository root before committing Rust/CLI changes:

```sh
cargo fmt --all -- --check
cargo clippy -p supervisor-core -p decent --all-targets --all-features -- -D warnings
cargo test -p supervisor-core -p decent
cargo build -p decent
./target/debug/decent-node --version
./target/debug/decent-node --help >/dev/null
```

For protocol changes:

```sh
cd packages/protocol
bun install --frozen-lockfile
bun run build
bun run test
```

For Tauri frontend changes:

```sh
cd apps/decent-app
bun install --frozen-lockfile
bun run build
bun run test
```

Run the affected focused gate during development, then every applicable gate
above before declaring the work complete. CI mirrors these commands.

## Versioning and releases

The components are independently versioned:

- `decent`: `bins/decent/Cargo.toml`
- `supervisor-core`: `crates/supervisor-core/Cargo.toml`
- Tauri app: `apps/decent-app/package.json` and `src-tauri/Cargo.toml`
- npm protocol: `packages/protocol/package.json`
- wire protocol: `PROTOCOL_VERSION` (independent of package versions)

Read `RELEASING.md` before changing a version or creating a tag. Never recover a
failed cargo-dist run by publishing a partial release manually. Fix/rerun the
workflow and verify the complete asset set.

## Repository conventions

- Rust 2021; format with rustfmt; Clippy warnings are errors.
- Bun for TypeScript packages and the Tauri frontend.
- Conventional commit scopes used here: `core`, `node`, `app`, `protocol`, `ci`,
  `docs`, `release`.
- Keep changes surgical. Do not reformat unrelated files except when restoring a
  red repository-wide formatting gate.
- Commit explicit paths. Push only when the current user/session explicitly asks.
- Never commit worker tokens, dispatch secrets, tenant keys, private payloads, or
  production URLs containing credentials.

## Operator-platform scope

The released node target is Apple Silicon macOS (`aarch64-apple-darwin`). Linux
and Windows support is not implied by portable core code. The Tauri app is
maintained but secondary; CLI/TUI is the primary node-local surface.

## Documentation map

- `README.md` — operator overview and quickstart.
- `CONTRIBUTING.md` — contributor setup and change procedures.
- `RELEASING.md` — node and protocol release runbooks.
- `CHANGELOG.md` — user-facing node changes.
- `packages/protocol/README.md` and `CHANGELOG.md` — protocol-specific contract
  and release history.
