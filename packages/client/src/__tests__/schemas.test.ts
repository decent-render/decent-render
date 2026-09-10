import {describe, expect, it} from 'vitest';
import {
  enqueueRenderResponseSchema,
  renderStatusResponseSchema,
  balanceResponseSchema,
  bundleUploadRequestSchema,
  versionsResponseSchema,
  webhookEventSchema,
} from '../schemas.js';

describe('public API schemas', () => {
  it('parses a completed render with a playable output and settled credits', () => {
    const parsed = renderStatusResponseSchema.parse({
      renderId: 'job-render-1',
      status: 'complete',
      progress: 1,
      outputUrl: 'https://cdn.example/output.mp4?signature=short-lived',
      creditsReserved: 5,
      creditsSettled: 5,
      error: null,
      createdAt: '2026-07-12T10:00:00.000Z',
      completedAt: '2026-07-12T10:01:00.000Z',
      verification: 'passed',
      outputSizeInBytes: 647399,
    });
    expect(parsed.outputUrl).toContain('.mp4');
    expect(parsed.creditsSettled).toBe(5);
  });

  it('rejects a completed render without outputUrl', () => {
    expect(() => renderStatusResponseSchema.parse({
      renderId: 'job-render-1', status: 'complete', progress: 1,
      outputUrl: null, creditsReserved: 5, creditsSettled: 5,
      error: null, createdAt: null, completedAt: null,
    })).toThrow();
  });

  // packet-48 OWED: rows completed before measured settlement existed
  // (pre-migration-0016) carry creditsSettled = null even on complete.
  it('parses a completed render with NULL settled credits (pre-measurement rows)', () => {
    const parsed = renderStatusResponseSchema.parse({
      renderId: 'job-render-2',
      status: 'complete',
      progress: 1,
      outputUrl: 'https://cdn.example/output.mp4?signature=short-lived',
      creditsReserved: 5,
      creditsSettled: null,
      error: null,
      createdAt: '2026-07-12T10:00:00.000Z',
      completedAt: '2026-07-12T10:01:00.000Z',
      verification: 'pending',
      outputSizeInBytes: null,
    });
    expect(parsed.status).toBe('complete');
    expect(parsed.creditsSettled).toBeNull();
  });

  it('requires verification on status and webhook payloads', () => {
    expect(() => renderStatusResponseSchema.parse({
      renderId: 'job-1', status: 'complete', progress: 1,
      outputUrl: 'https://cdn.test/out.mp4', creditsReserved: 5, creditsSettled: 5,
      error: null, createdAt: null, completedAt: null,
    })).toThrow();
    expect(webhookEventSchema.parse({
      event: 'render.verification', renderId: 'job-1', status: 'complete',
      outputUrl: 'https://cdn.test/out.mp4', error: null, creditsReserved: 5,
      creditsSettled: 5, verification: 'flagged',
      composition: {width: 1, height: 1, fps: 30, durationFrames: 1, codec: 'h264'},
      ts: '2026-07-12T10:00:00.000Z',
    }).verification).toBe('flagged');
  });

  // F2 accrued-cost protocol: mid-flight accrued cost is nullable (an old
  // node reports no measurements — null, never zero) and optional (a
  // dispatch predating the field omits it during the deploy window).
  it('parses mid-flight accrued cost when the farm reports it', () => {
    const parsed = renderStatusResponseSchema.parse({
      renderId: 'job-render-3', status: 'rendering', progress: 0.35,
      outputUrl: null, creditsReserved: 5, accruedCredits: 5, accruedAt: '2026-09-08T10:00:30.000Z',
      creditsSettled: null, error: null,
      createdAt: '2026-09-08T10:00:00.000Z', completedAt: null, verification: 'pending',
    });
    expect(parsed.accruedCredits).toBe(5);
    expect(parsed.accruedAt).toBe('2026-09-08T10:00:30.000Z');
  });

  it('parses accrued cost as null while a pre-F2 node renders (never zero)', () => {
    const parsed = renderStatusResponseSchema.parse({
      renderId: 'job-render-4', status: 'rendering', progress: 0.5,
      outputUrl: null, creditsReserved: 5, accruedCredits: null, accruedAt: null,
      creditsSettled: null, error: null,
      createdAt: '2026-09-08T10:00:00.000Z', completedAt: null, verification: 'pending',
    });
    expect(parsed.accruedCredits).toBeNull();
  });

  it('still parses status responses from a dispatch predating accrued fields', () => {
    const parsed = renderStatusResponseSchema.parse({
      renderId: 'job-render-5', status: 'rendering', progress: 0.5,
      outputUrl: null, creditsReserved: 5,
      creditsSettled: null, error: null,
      createdAt: '2026-09-08T10:00:00.000Z', completedAt: null, verification: 'pending',
    });
    expect(parsed.accruedCredits).toBeUndefined();
  });

  // F4 cancel-ack witness: when the node acknowledged the cancel (AFTER the
  // teardown join) and how long the teardown took. Nullable + optional so a
  // pre-F4 dispatch's responses still parse during the deploy window.
  it('parses the cancel-ack fields when the farm reports them', () => {
    const parsed = renderStatusResponseSchema.parse({
      renderId: 'job-render-6', status: 'canceled', progress: 0.6,
      outputUrl: null, creditsReserved: 5, accruedCredits: 5, accruedAt: '2026-09-08T10:00:30.000Z',
      cancelAckedAt: '2026-09-08T10:00:41.250Z', cancelTeardownMs: 812,
      creditsSettled: null, error: 'Render canceled by user',
      createdAt: '2026-09-08T10:00:00.000Z', completedAt: '2026-09-08T10:00:41.000Z', verification: 'pending',
    });
    expect(parsed.cancelAckedAt).toBe('2026-09-08T10:00:41.250Z');
    expect(parsed.cancelTeardownMs).toBe(812);
  });

  it('parses cancel-ack fields as null on a pre-F4 node (no ack ever arrives)', () => {
    const parsed = renderStatusResponseSchema.parse({
      renderId: 'job-render-7', status: 'canceled', progress: 0.6,
      outputUrl: null, creditsReserved: 5, accruedCredits: null, accruedAt: null,
      cancelAckedAt: null, cancelTeardownMs: null,
      creditsSettled: null, error: 'Render canceled by user',
      createdAt: '2026-09-08T10:00:00.000Z', completedAt: '2026-09-08T10:00:41.000Z', verification: 'pending',
    });
    expect(parsed.cancelAckedAt).toBeNull();
    expect(parsed.cancelTeardownMs).toBeNull();
  });

  it('still parses cancel responses from a dispatch predating the cancel-ack fields', () => {
    const parsed = renderStatusResponseSchema.parse({
      renderId: 'job-render-8', status: 'canceled', progress: 0.6,
      outputUrl: null, creditsReserved: 5,
      creditsSettled: null, error: 'Render canceled by user',
      createdAt: '2026-09-08T10:00:00.000Z', completedAt: '2026-09-08T10:00:41.000Z', verification: 'pending',
    });
    expect(parsed.cancelAckedAt).toBeUndefined();
    expect(parsed.cancelTeardownMs).toBeUndefined();
  });

  it('parses the active runner matrix', () => {
    const parsed = versionsResponseSchema.parse({
      supportedRemotionVersions: [{remotionVersion: '4.0.523', payloadVersion: 'runner-v1'}],
    });
    expect(parsed.supportedRemotionVersions).toHaveLength(1);
  });

  it('pins enqueue, balance, and bundle upload shapes', () => {
    expect(enqueueRenderResponseSchema.parse({renderId: 'job-1', status: 'pending', taskId: 'render-1', creditsReserved: 5}).status).toBe('pending');
    expect(balanceResponseSchema.parse({balance: 10, holds: 4, available: 6})).toEqual({balance: 10, holds: 4, available: 6});
    expect(bundleUploadRequestSchema.parse({sha256: 'a'.repeat(64), remotionVersion: '4.0.349', sizeBytes: 123}).sha256).toHaveLength(64);
  });
});
