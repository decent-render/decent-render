# Changelog

Notable changes to `@decent-render/protocol`. The package is versioned
independently of the wire `PROTOCOL_VERSION` (which stays **2**); the wire format
itself is governed by `fixtures/v2.json` (the shared Rust⇄TS contract).

## [Unreleased]

- Declare the tested Zod compatibility range (`>=3 <5`) and run development
  conformance against Zod 4, matching the current driffs consumer.
- Clarify source-of-truth wording: Rust and TypeScript are typed surfaces over
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
