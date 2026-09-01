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
 */
export const runnerEventSchema = z.discriminatedUnion('type', [
	z.object({
		type: z.literal('progress'),
		progress: z.number().min(0).max(1),
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
