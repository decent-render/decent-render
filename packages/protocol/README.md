# @decent-render/protocol

The wire protocol (**v2**) between the Decent render-network dispatch service and
a worker — TypeScript types + zod schemas.

## Canonical home

This package is the **TypeScript consumer API**, open-source in the
[decent-render](https://github.com/decent-render/decent-render) repo. Rust's
typed emitter/consumer lives in `crates/supervisor-core/src/protocol.rs`.
`fixtures/v2.json` is the shared wire-format truth: Rust locks the fixtures and
both sides assert every fixture round-trips with no field drift.

- TS: `src/__tests__/conformance.test.ts` (parses each fixture with zod,
  re-serializes, asserts deep field-set equality).
- Rust: `protocol::tests::cross_language_fixtures_round_trip` (parses each
  fixture into the typed enums, re-serializes, asserts deep value equality).

If either side drops or adds a field, its test fails. This is exactly the
tripwire that would have caught the `outputSizeInBytes` drift bug (the field
existed in TS, was missing on Rust, both had green tests, nothing compared them).

## Usage

```ts
import {
  WorkerMessageSchema,
  ServerMessageSchema,
  PROTOCOL_VERSION,
} from '@decent-render/protocol';

const msg = WorkerMessageSchema.parse(JSON.parse(raw));
```

`peerDependency`: `zod >=4 <5` (Zod 4 only). Zod 3 support was dropped in
0.1.1; the schema surface is tested against Zod 4 exclusively.

## Develop

```sh
cd packages/protocol
bun install
bunx vitest run          # TS conformance (26 tests)
```

Rust side, from the repo root:

```sh
cargo test -p supervisor-core cross_language
```

## Publishing

**OIDC trusted publishing — no npm token exists.** The publish is a
[GitHub Actions workflow](../../.github/workflows/publish-protocol.yml)
(`workflow_dispatch`) that authenticates to npm with a **short-lived, per-run
OIDC token** from GitHub — no long-lived token to steal, rotate, or store. npm
trusts *only* this specific workflow + repo + environment; combined with the
package setting "Require 2FA and disallow tokens", **no token can publish this
package, ever** — only this CI workflow can. (OpenSSF trusted-publishers
standard, same as PyPI/RubyGems.)

To publish a new version:

1. bump `version` in `packages/protocol/package.json`,
2. commit + push,
3. GitHub → Actions → **Publish protocol package** → Run workflow,
4. approve the `publish` environment prompt.

One-time setup (owner): on npmjs.com configure the package's **Trusted
Publisher** (GitHub Actions; org `decent-render`, repo `decent-render`, workflow
`publish-protocol.yml`, environment `publish`, action `npm publish`) and set
**Publishing access = "Require 2FA and disallow tokens (recommended)"**. On
GitHub, create the `publish` environment with Required reviewers = you. No
`NPM_TOKEN` secret is needed.

## Wire format

Plain JSON, camelCase keys, messages discriminated by `type`.

- **Worker → server:** `register`, `heartbeat`, `jobAccepted`, `jobProgress`,
  `jobComplete` (metrics: `wallMs`, `frames`, `outputSizeInBytes?`), `jobFailed`,
  `jobRejected` (the worker's refusal of an assignment it cannot honor).
- **Server → worker:** `jobAssign`, `cancel`, `ping`, `updateAvailable`.
- `jobAssign.attempt?` is an assignment lease echoed by accepted, progress,
  complete, and failed messages. It remains optional in protocol v2 so older
  supervisors and dispatch versions can be upgraded in either order.
- `jobAssign.browserSha256?` / `browserGetUrl?` pin the render browser: the
  worker downloads the tarball, verifies the sha, and reuses it across jobs.
  Optional for payloads that still bundle a browser under `chrome/`.
- `purgeAfter` is a `z.literal(true)` / Rust `PurgeAfter` — the privacy rule
  baked into the type (deserialization rejects `false`).

## Runner stdout contract (v1)

The runner inside a render payload talks to its supervisor over stdout NDJSON
lines (`progress`, `heartbeat`, `done`, `error`). That contract is pinned by
`fixtures/runner-stdout-v1.json` — an `accept` list (what both sides agree on
today) and a `reject` list (what must fail on both sides):

- **TS (emitter):** `@decent-render/runner-core` defines `runnerEventSchema`
  (`src/runner-stdout-schema.ts`); its conformance test asserts every fixture
  case, and the emitter's own tests validate every stdout frame against it
  (`src/__tests__/runner-stdout-conformance.test.ts`, `stdout-discipline.test.ts`,
  `liveness.test.ts`).
- **Rust (parser):** `RunnerEvent` in `crates/supervisor-core/src/runner.rs`
  round-trips the same file (`runner_stdout_fixtures_round_trip`).
- **CI** exercises both legs (runner-core's `bun run test` and the workspace
  `cargo test`) — read by relative path, never copied.

The first conformance run caught real drift: `RunnerEvent::Progress` accepted
any `f64`, while dispatch's protocol-v2 schema bounds progress to `[0, 1]` —
the parser now applies the same `unit_interval` bound.

## Payload-agnostic (future, not this version)

v2 is render-specific. The intended evolution — a **future** wire-version bump,
not now — is a `payloadType` discriminator on `jobAssign` so the same protocol
carries render + genAI payloads. The package is named `protocol` (not
`render-protocol`) to keep that door open; don't couple new fields to "render"
specifically.

## License

Apache-2.0.
