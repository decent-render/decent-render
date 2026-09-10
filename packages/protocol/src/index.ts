/**
 * Decent render-network wire protocol — protocol version 2.
 *
 * The transport-agnostic message contract between the dispatch service and a
 * worker. The worker opens ONE outbound WebSocket to the platform; jobs are
 * pushed down, heartbeats ride the same connection (GitHub-Actions-runner
 * pattern — works behind any NAT, no public worker addresses).
 *
 * CONTRACT HOME: this package is the TypeScript consumer surface; Rust's typed
 * emitter/consumer lives at `crates/supervisor-core/src/protocol.rs`.
 * `fixtures/v2.json` is the shared wire truth: Rust locks the fixtures and both
 * languages round-trip them. Neither side may change the wire alone — see
 * `__tests__/conformance.test.ts`.
 *
 * Every message carries `tenant` — driffs is tenant #1, but the protocol is
 * platform-agnostic by construction.
 *
 * Types + zod schemas only. No transport. Field names are camelCase on the wire;
 * messages are discriminated by the `type` field.
 */

import {z} from 'zod';

export const PROTOCOL_VERSION = 2;

// ── Worker → server ───────────────────────────────────────────────────────

/** First message after connect — the worker introduces itself. */
export const RegisterMessageSchema = z.object({
	type: z.literal('register'),
	tenant: z.string(),
	/**
	 * Pinned, not ranged: this package speaks exactly `PROTOCOL_VERSION`. A
	 * node announcing any other version is rejected at parse time instead of
	 * being treated as v2 (C-2). Rust pins the same way in protocol.rs.
	 */
	protocolVersion: z.literal(PROTOCOL_VERSION),
	/**
	 * ADVISORY ONLY — the verified operator identity comes from the signed
	 * worker token (operator claim), NOT from this field. Dispatch ignores
	 * this for identity purposes; it exists for protocol backward-compat.
	 * The actual operator on render_workers is set from conn.operator (the
	 * verified token claim) in dispatcher.ts.
	 */
	operator: z.string().nullable(),
	platform: z.enum(['company', 'community']),
	chip: z.string(),
	ramGb: z.number().int(),
	supervisorVersion: z.string(),
	payloadVersion: z.string(),
	/**
	 * What the node's HARDWARE can do — not what its operator is willing to do.
	 * `gpu` was previously wired to the supervisor's "accept real jobs" toggle,
	 * so any node with the switch on advertised itself as GPU-capable.
	 *
	 * The extra fields are optional so frames from supervisors predating them
	 * still parse; a missing `maxConcurrentJobs` means 1.
	 */
	capabilities: z.object({
		gpu: z.boolean(),
		maxConcurrentJobs: z.number().int().positive().optional(),
		/** Node's OS/arch, for matching platform-specific payloads. */
		os: z.string().optional(),
		arch: z.string().optional(),
	}),
});
export type RegisterMessage = z.infer<typeof RegisterMessageSchema>;

export const HeartbeatMessageSchema = z.object({
	type: z.literal('heartbeat'),
	tenant: z.string(),
	currentJobCount: z.number().int(),
});

export const JobAcceptedMessageSchema = z.object({
	type: z.literal('jobAccepted'),
	tenant: z.string(),
	jobId: z.string(),
	attempt: z.number().int().positive().optional(),
});

export const JobProgressMessageSchema = z.object({
	type: z.literal('jobProgress'),
	tenant: z.string(),
	jobId: z.string(),
	attempt: z.number().int().positive().optional(),
	progress: z.number().min(0).max(1),
	/**
	 * ACCRUED-COST PROTOCOL (F2) — RAW MEASUREMENTS, never money.
	 *
	 * A new runner reports how long it has been rendering (integer ms since
	 * render start) and how many frames it has finished (integer, derived
	 * progress × composition.durationInFrames). The DISPATCH service prices
	 * these with its own rate card to persist mid-flight accrued cost;
	 * the runner cannot price its own work and must never try — pricing is the
	 * platform's private concern and lives nowhere in this package.
	 *
	 * Both fields are optional so every node predating them stays a fully
	 * functional v2 peer: an old node's frames parse unchanged, and dispatch
	 * leaves accrued null rather than zero. NO PROTOCOL_VERSION bump — the
	 * pin is a register-time gate that would orphan every existing node
	 * (inspection-I10 §1.4); additive-optional fields are wire-tolerant in
	 * both directions.
	 */
	elapsedMs: z.number().int().nonnegative().optional(),
	framesSoFar: z.number().int().nonnegative().optional(),
});
export type JobProgressMessage = z.infer<typeof JobProgressMessageSchema>;

export const JobMetricsSchema = z.object({
	/** Wall-clock render time on the worker, milliseconds. */
	wallMs: z.number().int(),
	frames: z.number().int(),
	/** Output file size in bytes (optional — omitted when the runner didn't report it). */
	outputSizeInBytes: z.number().int().optional(),
});
export type JobMetrics = z.infer<typeof JobMetricsSchema>;

export const JobCompleteMessageSchema = z.object({
	type: z.literal('jobComplete'),
	tenant: z.string(),
	jobId: z.string(),
	attempt: z.number().int().positive().optional(),
	/** R2 key the worker uploaded the finished output to (via presigned PUT). */
	outputKey: z.string(),
	metrics: JobMetricsSchema,
});

export type JobCompleteMessage = z.infer<typeof JobCompleteMessageSchema>;

export const JobFailedMessageSchema = z.object({
	type: z.literal('jobFailed'),
	tenant: z.string(),
	jobId: z.string(),
	attempt: z.number().int().positive().optional(),
	reason: z.string(),
});
export type JobFailedMessage = z.infer<typeof JobFailedMessageSchema>;

/**
 * CANCEL-ACK WITNESS (F4) — the node→dispatch acknowledgement of a
 * dispatch-initiated cancel, sent AFTER the job task (teardown + purge) is
 * joined, never on cancel-frame receipt ("cancel accepted" is not "job
 * stopped", FARM-1 §1-C).
 *
 * `attempt` is REQUIRED — it is the provenance key. Dispatch's write
 * predicate is `status='canceled' ∧ attempts = attempt ∧
 * cancel_acked_at IS NULL`, so the ack is idempotent by (jobId, attempt):
 * a duplicate is absorbed, an ack for a re-assigned/other attempt matches
 * zero rows, and a requeued job can never inherit an earlier attempt's ack.
 * A job assigned without an attempt on the wire is acked as attempt 1 —
 * exactly what dispatch's `assignmentAttempt` already assumes for such
 * leases.
 *
 * `teardownMs` is the raw measurement the witness exists for: integer ms
 * from cancel-frame receipt to process-tree dead + workdir purge done.
 * Optional (int ≥ 0) so a node that cannot measure it stays a valid peer.
 *
 * Additive frame, `PROTOCOL_VERSION` stays 2: an old dispatch classifies an
 * unknown `type` as ignorable (inbound-frames.ts, once-per-type info log —
 * pinned by the dispatch census test), and old nodes simply never send it.
 */
export const JobCanceledAckMessageSchema = z.object({
	type: z.literal('jobCanceledAck'),
	tenant: z.string(),
	jobId: z.string(),
	attempt: z.number().int().positive(),
	teardownMs: z.number().int().nonnegative().optional(),
});
export type JobCanceledAckMessage = z.infer<typeof JobCanceledAckMessageSchema>;

/**
 * A job declined **without being started** — distinct from `jobFailed`, which
 * reports a render that ran and failed.
 *
 * Dispatch should requeue immediately and NOT count an attempt: no work was
 * consumed and nothing was lost. Before this existed a refusing node stayed
 * silent, and dispatch hard-failed the job after `max(10min, expected × 20)`.
 */
export const JobRejectedMessageSchema = z.object({
	type: z.literal('jobRejected'),
	tenant: z.string(),
	jobId: z.string(),
	attempt: z.number().int().positive().optional(),
	reason: z.enum(['not-accepting', 'busy']),
});
export type JobRejectedMessage = z.infer<typeof JobRejectedMessageSchema>;

export const WorkerMessageSchema = z.discriminatedUnion('type', [
	RegisterMessageSchema,
	HeartbeatMessageSchema,
	JobAcceptedMessageSchema,
	JobProgressMessageSchema,
	JobCompleteMessageSchema,
	JobFailedMessageSchema,
	JobRejectedMessageSchema,
	JobCanceledAckMessageSchema,
]);
export type WorkerMessage = z.infer<typeof WorkerMessageSchema>;

// ── Server → worker ─────────────────────────────────────────────────────────

/**
 * STILL RENDER DIRECTIVE (FARM-STILL) — present when the job renders exactly
 * ONE frame as a lossless PNG (`renderStill`) instead of a video
 * (`renderMedia`). Absent ⇒ today's video job, byte-identical behaviour.
 *
 * `frame` is the zero-based frame index to render; dispatch and the runner
 * both enforce `frame < durationFrames` (a still outside the composition is
 * unrenderable, and the refusal must be a parse-time rejection on BOTH sides,
 * not a mid-job failure — see the `reject` fixtures and the cross-field
 * refine below / serde validation in protocol.rs).
 *
 * `format` is the closed set `png` — the certification stills this carries
 * are lossless by contract; a lossy format would be a different (and
 * dishonest) job type. Unknown formats fail to parse.
 *
 * Additive-optional field: NO `PROTOCOL_VERSION` bump (same class as
 * jobProgress's elapsedMs/framesSoFar — old peers ignore/strips it; the
 * version pin is a register-time gate that would orphan the fleet).
 */
export const StillDirectiveSchema = z.object({
	frame: z.number().int().nonnegative(),
	format: z.literal('png'),
});
export type StillDirective = z.infer<typeof StillDirectiveSchema>;

/**
 * Job assignment. This is the privacy-rule carrier: assets arrive via presigned
 * R2 GET, output goes up via presigned PUT, and `purgeAfter` directs the
 * supervisor to wipe the working directory after the job. The device only ever
 * holds platform bundles + transient job assets — never persisted user content.
 */
export const JobAssignMessageSchema = z.object({
	type: z.literal('jobAssign'),
	tenant: z.string(),
	jobId: z.string(),
	attempt: z.number().int().positive().optional(),
	kind: z.enum(['standard', 'gpu']),
	durationFrames: z.number().int(),
	fps: z.number().int(),
	/**
	 * FARM-STILL R2: OPTIONAL — a still job has no video codec and dispatch
	 * sends none (the round-1 `'h264'` stand-in was pure old-schema
	 * compatibility; no legacy to carry). A VIDEO job (no `still`) must
	 * still carry it — enforced by the second refine below on BOTH sides.
	 */
	codec: z.enum(['h264', 'vp8']).optional(),
	/**
	 * Pinned platform bundle (content-addressed tar.gz of the Remotion webpack
	 * bundle). The worker downloads it via the presigned GET, verifies the
	 * sha256, extracts, and renders against the extracted dir as `serveUrl`.
	 * Content-addressing makes the render reproducible (a redeploy can never
	 * mutate an in-flight job's bundle) — required for community-tier SSIM
	 * verification later.
	 */
	bundleSha256: z.string(),
	bundleGetUrl: z.string(),
	/**
	 * Pinned render payload tarball (runner binary + remotion-binaries/). Dispatch
	 * resolves this from render_bundles.remotionVersion to an active payload row;
	 * workers verify and cache by sha.
	 */
	payloadSha256: z.string(),
	payloadGetUrl: z.string(),
	/**
	 * Pinned browser tarball, cached separately from the payload.
	 *
	 * The browser is ~170MB and is identical across Remotion versions that pin
	 * the same Chrome build, so bundling it into every payload would re-ship it
	 * per Remotion version per platform. Splitting it out means an operator
	 * downloads a given Chrome once, no matter how many payloads reference it.
	 *
	 * The tarball root contains an `executable` file holding the browser's path
	 * relative to that root — the publisher knows exactly what it downloaded, so
	 * the supervisor never guesses a platform-specific nested layout.
	 *
	 * Optional for compatibility with payloads that still bundle their own
	 * browser under `chrome/`. When absent the runner falls back to that
	 * in-payload manifest; when BOTH are absent Remotion downloads ~1GB into the
	 * per-job workdir and loses it to the purge on every job.
	 */
	browserSha256: z.string().optional(),
	browserGetUrl: z.string().optional(),
	/**
	 * Presigned R2 GET for the job's input props JSON:
	 * `{compositionId, inputProps}`. Self-describing — the worker needs no other
	 * job data.
	 */
	inputPropsGetUrl: z.string(),
	/** Presigned R2 GET URLs for input assets. */
	assetGetUrls: z.array(z.string()),
	/** Presigned R2 PUT URL the worker uploads the finished mp4 to. */
	outputPutUrl: z.string(),
	/** R2 key the output lands at (so the server can resolve it post-upload). */
	outputKey: z.string(),
	/** Supervisor MUST purge the working directory after the job. Always true. */
	purgeAfter: z.literal(true),
	/**
	 * STILL RENDER DIRECTIVE (FARM-STILL) — optional; absent ⇒ video job.
	 * `frame` must be `< durationFrames` (validated cross-field below AND in
	 * Rust's Deserialize for JobAssignMessage — neither side may accept it).
	 */
	still: StillDirectiveSchema.optional(),
})
	// Cross-field bound, ON the schema so every parse path refuses a still
	// outside the composition at parse time (fix1 P3-2 wording): fixture
	// conformance on both languages, the DISPATCH SENDER GATE at SEND time —
	// `apps/dispatch/src/outbound-frame.ts` (`rejectServerFrame`/`safeParse`)
	// validates every server frame before it hits the WebSocket; the frame
	// leaves `assignJobToWorker` unvalidated — and the supervisor's receive
	// parse. durationFrames stays ≥ 1 (schema compatibility — the tenant
	// sends the composition's real duration; the still renders ONE frame of
	// it).
	.refine(
		(assign) => !assign.still || assign.still.frame < assign.durationFrames,
		{message: 'still.frame must be < durationFrames', path: ['still', 'frame']},
	)
	// A video job renders WITH a codec; only a still may omit one.
	.refine((assign) => assign.still !== undefined || assign.codec !== undefined, {
		message: 'codec is required unless the job carries a still directive',
		path: ['codec'],
	});
export type JobAssignMessage = z.infer<typeof JobAssignMessageSchema>;

export const CancelMessageSchema = z.object({
	type: z.literal('cancel'),
	tenant: z.string(),
	jobId: z.string(),
});

export const PingMessageSchema = z.object({
	type: z.literal('ping'),
	tenant: z.string(),
});

export const UpdateAvailableMessageSchema = z.object({
	type: z.literal('updateAvailable'),
	tenant: z.string(),
	supervisorVersion: z.string(),
	payloadVersion: z.string(),
});

export const ServerMessageSchema = z.discriminatedUnion('type', [
	JobAssignMessageSchema,
	CancelMessageSchema,
	PingMessageSchema,
	UpdateAvailableMessageSchema,
]);
export type ServerMessage = z.infer<typeof ServerMessageSchema>;
