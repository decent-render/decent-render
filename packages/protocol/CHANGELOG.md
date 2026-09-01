# Changelog

Notable changes to `@decent-render/protocol`. The package is versioned
independently of the wire `PROTOCOL_VERSION` (which stays **2**); the wire format
itself is governed by `fixtures/v2.json` (the shared Rust⇄TS contract).

## [Unreleased]

- `register.protocolVersion` is now `z.literal(PROTOCOL_VERSION)` instead of any
  integer: a node announcing a version this package does not speak fails to
  parse rather than being treated as v2. Rust pins identically. No wire change.
- `fixtures/v2.json` gained a `reject` array (negative contract): entries both
  languages must FAIL to parse (`protocolVersion: 99`, `progress: 1.5`,
  `progress: -0.1`, `codec: 'av1'`). The conformance suites also assert both
  fixture sets are non-empty, so an empty or unreadable fixture file can no
  longer pass vacuously.

- Added optional `browserSha256` / `browserGetUrl` to `jobAssign`. The browser is
  now a standalone content-addressed artifact rather than part of the render
  payload: it is ~170MB and identical across Remotion versions that pin the same
  Chrome (4.0.487 and 4.0.506 both pin 149.0.7790.0), so bundling re-shipped it
  per version per platform. Absent fields mean the payload carries its own
  browser under `chrome/`, so existing payloads keep working.

- Added an optional positive `attempt` lease to `jobAssign` and its
  `jobAccepted`, `jobProgress`, `jobComplete`, and `jobFailed` responses. New
  supervisors echo the lease so dispatch can reject delayed messages from an
  older assignment; attempt-less v2 frames remain accepted during rollout.

## [0.1.1] — 2026-07-10

- **Zod 4 only.** Narrowed the peer-dependency range from `>=3 <5` to `>=4 <5`.
  Zod 3 is no longer a supported consumer. The schema surface was already
  developed and tested against Zod 4; this makes the package declaration
  honest and prevents silent Zod-3 resolution.
- Clarified source-of-truth wording: Rust and TypeScript are typed surfaces over
  the shared fixture contract; neither may change the wire alone.

## [0.1.0] — 2026-07-08

- Initial extraction: TS types + zod schemas for protocol **v2**, moved out of
  driffs' `src/lib/render-farm/protocol.ts`. The canonical TS home is now this
  open package; the Rust canonical remains
  `crates/supervisor-core/src/protocol.rs`.
- **Cross-language conformance test** — `fixtures/v2.json` (the shared
  wire-format contract) asserted by both the TS suite (`conformance.test.ts`)
  and the Rust test (`cross_language_fixtures_round_trip`). Either side dropping
  or adding a field fails its test. Covers the `outputSizeInBytes` drift scar
  both ways (present + absent fixtures).
- Payload-agnostic seam documented (future wire-version bump; not this version).
