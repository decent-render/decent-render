import {z} from 'zod';

export const sha256Schema = z.string().regex(/^[0-9a-f]{64}$/);
export const remotionVersionSchema = z.string().regex(/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/);
export const renderStatusSchema = z.enum(['pending', 'assigned', 'rendering', 'complete', 'failed', 'canceled']);
export const verificationStatusSchema = z.enum(['pending', 'passed', 'flagged']);
export type VerificationStatus = z.infer<typeof verificationStatusSchema>;
export const isoDateSchema = z.string().datetime().nullable();

export const enqueueRenderRequestSchema = z.object({
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
  inputAssetKeys: z.array(z.string()).default([]),
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
  creditsSettled: z.number().int().nonnegative(),
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
    codec: z.enum(['h264', 'vp8']),
  }),
  ts: z.string().datetime(),
});
export type WebhookEvent = z.infer<typeof webhookEventSchema>;

export const apiErrorSchema = z.object({
  error: z.string(),
  code: z.string().optional(),
  supportedRemotionVersions: z.array(z.string()).optional(),
}).passthrough();
