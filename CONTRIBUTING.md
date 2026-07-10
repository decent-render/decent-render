# Contributing to Decent Render

Thank you for helping improve the open operator side of the Decent render
network. Read `AGENTS.md` first; its privacy, protocol, and real-job gates are
part of the contribution contract.

## Prerequisites

- Apple Silicon macOS for node/launchd and real-render validation
- Stable Rust toolchain with Cargo
- Bun 1.3+
- Xcode command-line tools
- Tauri system prerequisites only when changing `apps/decent-app`

Most unit and conformance work does not require access to the private dispatch,
credit ledger, or render payloads.

## Bootstrap

```sh
git clone https://github.com/decent-render/decent-render.git
cd decent-render
cargo build -p decent
(cd packages/protocol && bun install --frozen-lockfile)
(cd apps/decent-app && bun install --frozen-lockfile)
```

## Development gates

Rust/CLI:

```sh
cargo fmt --all -- --check
cargo clippy -p supervisor-core -p decent --all-targets --all-features -- -D warnings
cargo test -p supervisor-core -p decent
cargo build -p decent
./target/debug/decent-node --version
./target/debug/decent-node --help >/dev/null
```

Protocol:

```sh
cd packages/protocol
bun run build
bun run test
```

Tauri frontend:

```sh
cd apps/decent-app
bun run build
bun run test
```

## Protocol changes

A wire change is incomplete until all of these move together:

1. Update Rust types in `crates/supervisor-core/src/protocol.rs`.
2. Update TS schemas/types in `packages/protocol/src/index.ts`.
3. Update `packages/protocol/fixtures/v2.json` from the Rust contract.
4. Run both conformance suites.
5. Decide whether `PROTOCOL_VERSION` must change.
6. Add a protocol changelog entry.
7. Verify old/new version rejection or compatibility behavior explicitly.

Do not hand-mirror a change without updating shared fixtures. Do not treat a
package-version bump as a wire-version bump.

## Testing safety

- `allow_real_jobs` remains false by default.
- Use fixture URLs and synthetic credentials in tests.
- Never point automated tests at production dispatch or customer artifacts.
- Real-render smoke tests require an explicit operator session and must confirm
  purge after success, failure, and cancel.

## Pull requests

A focused pull request should include:

- the problem and invariant being protected;
- tests proving the behavior or regression;
- commands run and exact result;
- protocol fixture/changelog changes when applicable;
- explicit disclosure of anything requiring manual macOS or live-network proof.

Use conventional commits with a focused scope, for example:

- `fix(core): purge workdir after canceled payload`
- `feat(node): expose live daemon status`
- `test(protocol): lock optional metrics field`

## Security

Do not open a public issue containing credentials, customer content, or an
exploitable vulnerability. Follow `SECURITY.md` once present; until then, contact
the repository owner privately through GitHub.
