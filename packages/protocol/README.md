# @decent-render/protocol

The wire protocol (**v2**) between the Decent render-network dispatch service and
a worker — TypeScript types + zod schemas.

## Canonical home

This package is the **TS source of truth**, open-source in the
[decent-render](https://github.com/decent-render/decent-render) repo. The **Rust
canonical** is `crates/supervisor-core/src/protocol.rs`. The two are kept in sync
by a **cross-language conformance test**: `fixtures/v2.json` is the shared
wire-format contract, and both sides assert every fixture round-trips with no
field drift.

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

`peerDependency`: `zod ^3`.

## Develop

```sh
cd packages/protocol
bun install
bunx vitest run          # TS conformance (13 tests)
```

Rust side, from the repo root:

```sh
cargo test -p supervisor-core cross_language
```

## Wire format

Plain JSON, camelCase keys, messages discriminated by `type`.

- **Worker → server:** `register`, `heartbeat`, `jobAccepted`, `jobProgress`,
  `jobComplete` (metrics: `wallMs`, `frames`, `outputSizeInBytes?`), `jobFailed`.
- **Server → worker:** `jobAssign`, `cancel`, `ping`, `updateAvailable`.
- `purgeAfter` is a `z.literal(true)` / Rust `PurgeAfter` — the privacy rule
  baked into the type (deserialization rejects `false`).

## Payload-agnostic (future, not this version)

v2 is render-specific. The intended evolution — a **future** wire-version bump,
not now — is a `payloadType` discriminator on `jobAssign` so the same protocol
carries render + genAI payloads. The package is named `protocol` (not
`render-protocol`) to keep that door open; don't couple new fields to "render"
specifically.

## License

Apache-2.0.
