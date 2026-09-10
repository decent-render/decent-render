import {z} from 'zod';

export const sha256Schema = z.string().regex(/^[0-9a-f]{64}$/);
export const remotionVersionSchema = z.string().regex(/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/);
export const renderStatusSchema = z.enum(['pending', 'assigned', 'rendering', 'complete', 'failed', 'canceled']);
export const verificationStatusSchema = z.enum(['pending', 'passed', 'flagged']);
export type VerificationStatus = z.infer<typeof verificationStatusSchema>;
export const isoDateSchema = z.string().datetime().nullable();

/**
 * STILL DIRECTIVE (FARM-STILL) — render ONE frame of the composition as a
 * lossless PNG instead of a video. `frame` is the zero-based frame index and
 * must be `< durationFrames` (the composition's REAL duration — the still
 * renders one frame OF it); the cross-field refine below enforces that on
 * both the SDK and the dispatch front door, and the wire/runner enforce it
 * again at parse time. The format set is closed at png: certification stills
 * are lossless by contract.
 */
export const stillDirectiveSchema = z.object({
  frame: z.number().int().nonnegative(),
  format: z.literal('png'),
});
export type StillDirective = z.infer<typeof stillDirectiveSchema>;

export const enqueueRenderRequestSchema = z
  .object({
  bundleSha256: sha256Schema,
  inputProps: z.unknown().optional(),
  compositionId: z.string().min(1).default('Main'),
  compositionWidth: z.number().int().positive(),
  compositionHeight: z.number().int().positive(),
  fps: z.number().positive(),
  durationFrames: z.number().int().positive(),
  codec: z.enum(['h264', 'vp8']).default('h264'),
  kind: z.enum(['standard', 'gpu']).default('standard'),
  tier: z.enum(['cloud', 'community']).default('cloud'),
  communityConsented: z.boolean().default(false),
  targetOperator: z.string().min(1).optional(),
  // Intent to render free on infrastructure the caller is entitled to. The farm
  // gates this by a server-side workspace allowlist (a non-entitled workspace is
  // rejected, never silently charged) and, when granted, enqueues the job
  // untargeted so it routes to the company fleet. Distinct from targetOperator,
  // which pins a job to a specific community operator's own device.
  selfRender: z.boolean().optional(),
  inputAssetKeys: z.array(z.string()).default([]),
  // FARM-STILL: absent ⇒ video job, byte-identical request. Present ⇒ the
  // farm renders ONE frame as a lossless PNG and the output is
  // `still-f<frame>.png`. codec still defaults and is carried but is INERT
  // for stills (the wire/runner branch on `still`, not on codec).
  still: stillDirectiveSchema.optional(),
  })
  // Cross-field bound, ON the schema: the SAME schema is the dispatch front
  // door's validator (apps/dispatch/src/api.ts), so a still outside the
  // composition is refused identically at both ends — one schema, one
  // validator, one enqueue path.
  .refine((request) => !request.still || request.still.frame < request.durationFrames, {
    message: 'still.frame must be < durationFrames',
    path: ['still', 'frame'],
  });
export type EnqueueRenderRequest = z.infer<typeof enqueueRenderRequestSchema>;

export const enqueueRenderResponseSchema = z.object({
  renderId: z.string(),
  status: z.literal('pending'),
  taskId: z.string(),
  creditsReserved: z.number().int().nonnegative(),
});
export type EnqueueRenderResponse = z.infer<typeof enqueueRenderResponseSchema>;

const renderStatusBase = z.object({
  renderId: z.string(),
  progress: z.number().min(0).max(1).nullable(),
  creditsReserved: z.number().int().nonnegative().nullable(),
  /**
   * ACCRUED-COST PROTOCOL (F2): mid-flight accrued cost, priced by the
   * farm from the node's raw measurements (elapsedMs/framesSoFar on
   * jobProgress) — nullable, and OPTIONAL so responses from a dispatch
   * predating the field still parse during the deploy window. NULL (never
   * zero) while a pre-F2 node renders: an old node reports no measurements
   * and "no figure" is not "free". Terminal money stays creditsSettled.
   */
  accruedCredits: z.number().int().nonnegative().nullable().optional(),
  /** When accruedCredits was last computed (dispatch-side timestamp). */
  accruedAt: isoDateSchema.optional(),
  /**
   * CANCEL-ACK WITNESS (F4): when the node acknowledged the dispatch's
   * cancel — sent AFTER the job task (teardown + purge) joined, never on
   * cancel-frame receipt. Nullable, and OPTIONAL so responses from a
   * dispatch predating the field still parse during the deploy window.
   * NULL forever on rows canceled by a pre-F4 node (no ack ever arrives).
   */
  cancelAckedAt: isoDateSchema.optional(),
  /**
   * The node-measured teardown duration (ms): cancel-frame receipt →
   * process tree dead + workdir purge done (F4). Nullable + optional for
   * the same deploy-window reason as cancelAckedAt.
   */
  cancelTeardownMs: z.number().int().nonnegative().nullable().optional(),
  error: z.string().nullable(),
  createdAt: isoDateSchema,
  completedAt: isoDateSchema,
  verification: verificationStatusSchema,
});
const nonCompleteStatus = renderStatusBase.extend({
  status: z.enum(['pending', 'assigned', 'rendering', 'failed', 'canceled']),
  outputUrl: z.null(),
  creditsSettled: z.number().int().nonnegative().nullable(),
});
const completeStatus = renderStatusBase.extend({
  status: z.literal('complete'),
  progress: z.literal(1),
  outputUrl: z.string().url(),
  // packet-48 OWED: rows completed before measured settlement existed
  // (pre-migration-0016) settle nothing — null, not zero.
  creditsSettled: z.number().int().nonnegative().nullable(),
  /**
   * MEASURED object size (dispatch HEADs the output before completing the
   * job). Nullable — mirrors the nullable column for rows whose HEAD
   * reported no size — never optional: the R2 deploy-window hedge is gone
   * (no legacy dispatches matter). `renderStillOnFarm` requires a NUMBER
   * and throws a client-kind error on null.
   */
  outputSizeInBytes: z.number().int().nonnegative().nullable(),
});
export const renderStatusResponseSchema = z.discriminatedUnion('status', [completeStatus, nonCompleteStatus]);
export type RenderStatusResponse = z.infer<typeof renderStatusResponseSchema>;

export const cancelRenderResponseSchema = z.object({renderId: z.string(), status: z.literal('canceled')});
export type CancelRenderResponse = z.infer<typeof cancelRenderResponseSchema>;

export const balanceResponseSchema = z.object({
  balance: z.number().int(),
  holds: z.number().int().nonnegative(),
  available: z.number().int(),
});
export type BalanceResponse = z.infer<typeof balanceResponseSchema>;

export const bundleUploadRequestSchema = z.object({
  sha256: sha256Schema,
  remotionVersion: remotionVersionSchema,
  sizeBytes: z.number().int().positive(),
});
export type BundleUploadRequest = z.infer<typeof bundleUploadRequestSchema>;
export const bundleUploadResponseSchema = z.object({
  sha256: sha256Schema,
  uploadUrl: z.string().url().nullable(),
  expiresAt: z.string().datetime().nullable(),
  alreadyRegistered: z.boolean(),
});
export type BundleUploadResponse = z.infer<typeof bundleUploadResponseSchema>;

export const latestBundleResponseSchema = z.object({sha256: sha256Schema});
export type LatestBundleResponse = z.infer<typeof latestBundleResponseSchema>;

export const workerAvailabilityResponseSchema = z.object({anyConnected: z.boolean()});
export type WorkerAvailabilityResponse = z.infer<typeof workerAvailabilityResponseSchema>;
export const bundleCompleteResponseSchema = z.object({
  sha256: sha256Schema,
  remotionVersion: remotionVersionSchema,
  registered: z.literal(true),
});
export type BundleCompleteResponse = z.infer<typeof bundleCompleteResponseSchema>;

export const versionsResponseSchema = z.object({
  supportedRemotionVersions: z.array(z.object({
    remotionVersion: remotionVersionSchema,
    payloadVersion: z.string().min(1),
  })),
});
export type VersionsResponse = z.infer<typeof versionsResponseSchema>;

export const webhookEndpointSchema = z.object({
  id: z.string(), url: z.string().url(), isActive: z.boolean(), createdAt: isoDateSchema,
});
export const webhookListResponseSchema = z.object({endpoints: z.array(webhookEndpointSchema)});
export const webhookCreateRequestSchema = z.object({url: z.string().url()});
export const webhookCreateResponseSchema = webhookEndpointSchema.extend({secret: z.string().startsWith('whsec_')});
export const webhookUpdateRequestSchema = z.object({url: z.string().url().optional(), isActive: z.boolean().optional()}).refine((value) => Object.keys(value).length > 0);
export const webhookDeleteResponseSchema = z.object({deleted: z.literal(true)});
export type WebhookEndpoint = z.infer<typeof webhookEndpointSchema>;

export const webhookEventSchema = z.object({
  event: z.enum(['render.complete', 'render.failed', 'render.canceled', 'render.verification']),
  renderId: z.string(),
  status: z.enum(['complete', 'failed', 'canceled']),
  outputUrl: z.string().url().nullable(),
  error: z.string().nullable(),
  creditsReserved: z.number().int().nonnegative().nullable(),
  creditsSettled: z.number().int().nonnegative().nullable(),
  verification: verificationStatusSchema,
  composition: z.object({
    width: z.number().int().positive(),
    height: z.number().int().positive(),
    fps: z.number().positive(),
    durationFrames: z.number().int().positive(),
    // Nullable for still jobs (FARM-STILL): a still has no video codec —
    // the honest value is null, never a fake 'h264'. Not optional: dispatch
    // always sends the key (the R2 deploy-window hedge is gone).
    codec: z.enum(['h264', 'vp8']).nullable(),
  }),
  ts: z.string().datetime(),
});
export type WebhookEvent = z.infer<typeof webhookEventSchema>;

export const apiErrorSchema = z.object({
  error: z.string(),
  code: z.string().optional(),
  supportedRemotionVersions: z.array(z.string()).optional(),
}).passthrough();
