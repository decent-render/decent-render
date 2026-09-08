import {z} from 'zod';

/**
 * The runner→supervisor stdout contract (v1), as a zod schema.
 *
 * The runner (this package) emits NDJSON lines on stdout; the supervisor
 * (crates/supervisor-core/src/runner.rs, `RunnerEvent`) parses them. Both
 * sides are pinned by the shared fixture set
 * `packages/protocol/fixtures/runner-stdout-v1.json`:
 *
 * - TS: src/__tests__/runner-stdout-conformance.test.ts (this schema parses
 *   every accept case and rejects every reject case; the EMITTER's own tests
 *   assert everything written to stdout passes this schema).
 * - Rust: runner.rs `runner_stdout_fixtures_round_trip` (same file, same
 *   accept/reject split, `serde_json` into `RunnerEvent`).
 *
 * Bounds mirror the supervisor: `progress` is a fraction in [0, 1]
 * (protocol v2's jobProgress carries the identical bound — dispatch refuses
 * to persist anything outside it), and `done` ALWAYS carries
 * `outputSizeInBytes` (the supervisor stamps it onto the metrics it
 * persists). `metrics` inside `done` is optional: current runners always
 * send it, legacy runners never did, and the supervisor fills the envelope
 * values in either way.
 *
 * ACCRUED-COST PROTOCOL (F2): `progress` optionally carries `elapsedMs`
 * (integer ms since render start) and `framesSoFar` (integer frames done,
 * derived from progress × composition.durationInFrames). RAW MEASUREMENTS
 * ONLY — never money, never a price. Pricing is the dispatch service's
 * private concern; the runner must not compute or carry it. Both fields are
 * optional so frames from runners predating them still parse (an old node
 * simply reports nothing and dispatch leaves accrued null).
 */
export const runnerEventSchema = z.discriminatedUnion('type', [
	z.object({
		type: z.literal('progress'),
		progress: z.number().min(0).max(1),
		/** Integer milliseconds since render start (Date.now() - started). */
		elapsedMs: z.number().int().nonnegative().optional(),
		/** Integer frames rendered so far — floor(progress × durationInFrames). */
		framesSoFar: z.number().int().nonnegative().optional(),
	}),
	z.object({
		type: z.literal('heartbeat'),
	}),
	z.object({
		type: z.literal('done'),
		outputSizeInBytes: z.number().int().nonnegative(),
		wallTimeMs: z.number().int().nonnegative(),
		metrics: z
			.object({
				wallMs: z.number().int().nonnegative(),
				frames: z.number().int().nonnegative(),
				outputSizeInBytes: z.number().int().nonnegative().optional(),
			})
			.optional(),
	}),
	z.object({
		type: z.literal('error'),
		message: z.string().min(1),
	}),
]);

export type RunnerEvent = z.infer<typeof runnerEventSchema>;
