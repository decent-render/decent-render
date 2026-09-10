# Changelog

Notable changes to `@decent-render/protocol`. The package is versioned
independently of the wire `PROTOCOL_VERSION` (which stays **2**); the wire format
itself is governed by `fixtures/v2.json` (the shared Rust⇄TS contract).

## [Unreleased]

- **Still render directive (FARM-STILL)** — `jobAssign` gains one OPTIONAL
  field, `still: {frame, format}`. Present ⇒ the job renders exactly ONE
  frame as a lossless PNG (`renderStill`) instead of a video; absent ⇒
  today's video job, byte-identical behaviour. `frame` is zero-based and
  must be `< durationFrames` — enforced at PARSE time on both sides (a TS
  cross-field refine ON `JobAssignMessageSchema`, and Rust's hand-written
  `Deserialize` over a derive-only `JobAssignMessageUnchecked` mirror), with
  reject fixtures pinning both the `frame == durationFrames` boundary and
  the closed `format: 'png'` set. `durationFrames` itself is untouched
  (≥ 1; the tenant sends the composition's real duration). NO
  `PROTOCOL_VERSION` bump — additive-optional field, wire-tolerant in both
  directions (old dispatch never sends it; old nodes ignore it). Old-node
  rollout caveat (dispatch-side, not wire): a pre-still node that receives
  a still frame will render a VIDEO and the job fails at output
  verification — republish payloads before enqueueing stills.
- `fixtures/v2.json`: +1 accept case (jobAssign with `still`), +2 reject
  cases (still.frame at durationFrames; unknown format 'jpeg').

- **Cancel-ack witness (F4)** — new worker→server frame `jobCanceledAck
  {tenant, jobId, attempt, teardownMs?}`: the node's acknowledgement of a
  dispatch-initiated cancel, emitted after the job task (teardown +
  purge) is joined. `attempt` is REQUIRED — the provenance key, making
  dispatch's persist idempotent by (jobId, attempt) — and `teardownMs`
  (optional int ≥ 0) is the raw measurement: ms from cancel receipt to
  process tree dead + purge done. Fixtures carry the PRESENT/ABSENT
  teardownMs pair plus three rejects (attempt missing, attempt as string,
  negative teardownMs). No `PROTOCOL_VERSION` bump — additive frame,
  tolerated by old dispatches (unknown type → ignored) and never sent by
  old nodes.

- **F2 verify fix (F-6)** — the pricing grep pin is widened: the bare word
  `pricing` is now banned in runner-core/protocol/supervisor-core non-test
  source (the brief's check was "the word → 0"; the pin only banned
  `pricing.ts`, so six "pricing does not happen here" prose lines
  survived). Each surviving line is on a documented exact-line allow-list
  in the test; a new use of the word fails the pin until consciously
  allow-listed, and a stale entry fails the freshness assertion, so the
  list cannot rot.

- **Accrued-cost protocol (F2)** — `jobProgress` gains two OPTIONAL fields,
  `elapsedMs` (integer ms since render start) and `framesSoFar` (integer
  frames finished). RAW MEASUREMENTS ONLY — the runner/supervisor report
  them, dispatch prices them with its private rate card; no pricing
  vocabulary may ever appear in this package (pinned by a new grep test).
  Both directions stay wire-tolerant: an old node's frame without the fields
  parses unchanged, and a new node's frame is stripped to its known-field
  shape by an older dispatch's zod objects. `PROTOCOL_VERSION` stays **2** —
  now pinned by explicit tests on both sides (a bump is a register-time gate
  that orphans every existing node; additive-optional fields need no bump).
- `fixtures/v2.json`: +1 accept case (jobProgress with the measurements),
  +1 reject case (negative framesSoFar). `fixtures/runner-stdout-v1.json`:
  +2 accept cases (with fields / elapsedMs only) and +3 reject cases
  (negative elapsedMs, non-integer framesSoFar, string framesSoFar).

## [0.1.3] — 2026-09-02

- `fixtures/runner-stdout-v1.json`: the runner→supervisor stdout contract
  (`progress` / `heartbeat` / `done` / `error` NDJSON lines) now has a shared
  accept/reject fixture set, parsed by `@decent-render/runner-core`'s zod
  schema and by `supervisor-core`'s `RunnerEvent` in CI. The first run caught
  the Rust parser accepting out-of-range `progress`; it now applies the same
  `[0, 1]` bound as `jobProgress`.
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
