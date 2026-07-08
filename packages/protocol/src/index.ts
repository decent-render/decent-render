/**
 * Decent render-network wire protocol — protocol version 2.
 *
 * The transport-agnostic message contract between the dispatch service and a
 * worker. The worker opens ONE outbound WebSocket to the platform; jobs are
 * pushed down, heartbeats ride the same connection (GitHub-Actions-runner
 * pattern — works behind any NAT, no public worker addresses).
 *
 * CANONICAL HOME: this package (`@decent-render/protocol`) is the TS source of
 * truth, living open-source in the decent-render repo. The Rust canonical lives
 * at `crates/supervisor-core/src/protocol.rs`. A cross-language conformance test
 * (Rust emits golden fixtures in `fixtures/`; this package's test asserts TS
 * matches them) keeps the two from drifting — see `__tests__/conformance.test.ts`.
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
	protocolVersion: z.number().int(),
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
	capabilities: z.object({gpu: z.boolean()}),
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
});

export const JobProgressMessageSchema = z.object({
	type: z.literal('jobProgress'),
	tenant: z.string(),
	jobId: z.string(),
	progress: z.number().min(0).max(1),
});

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
	/** R2 key the worker uploaded the finished output to (via presigned PUT). */
	outputKey: z.string(),
	metrics: JobMetricsSchema,
});

export type JobCompleteMessage = z.infer<typeof JobCompleteMessageSchema>;

export const JobFailedMessageSchema = z.object({
	type: z.literal('jobFailed'),
	tenant: z.string(),
	jobId: z.string(),
	reason: z.string(),
});
export type JobFailedMessage = z.infer<typeof JobFailedMessageSchema>;

export const WorkerMessageSchema = z.discriminatedUnion('type', [
	RegisterMessageSchema,
	HeartbeatMessageSchema,
	JobAcceptedMessageSchema,
	JobProgressMessageSchema,
	JobCompleteMessageSchema,
	JobFailedMessageSchema,
]);
export type WorkerMessage = z.infer<typeof WorkerMessageSchema>;

// ── Server → worker ─────────────────────────────────────────────────────────

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
	kind: z.enum(['standard', 'gpu']),
	durationFrames: z.number().int(),
	fps: z.number().int(),
	codec: z.enum(['h264', 'vp8']),
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
