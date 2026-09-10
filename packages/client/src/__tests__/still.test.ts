/**
 * FARM-STILL — the client surface for single-frame PNG renders.
 *
 * Pinned here:
 * - `enqueueRenderRequestSchema` accepts the optional `still` directive and
 *   REFUSES `still.frame >= durationFrames` (the same schema is the dispatch
 *   front door's validator — one schema, one validator, one enqueue path);
 * - unknown still formats are refused (the certification contract is
 *   lossless PNG only);
 * - a video request WITHOUT `still` parses to a request that has NO still
 *   key (byte-identical behaviour for existing tenants);
 * - `renderStillOnFarm` enqueues with the directive, polls to complete, and
 *   returns {url, sizeInBytes, renderId, frame, verification, creditsSettled};
 * - a completed still WITHOUT a measured size is a named client error, not
 *   a guessed figure;
 * - `chromiumOptions.gl` other than 'angle' is refused CLIENT-side before
 *   any network call (the farm honors no per-job gl override yet);
 * - the complete status schema carries `outputSizeInBytes` (optional — old
 *   dispatch responses still parse) and the webhook composition codec is
 *   honestly nullable for still rows.
 */
import {afterEach, describe, expect, it, vi} from 'vitest';
import {
  FarmApiError,
  enqueueRender,
  renderStillOnFarm,
} from '../index.js';
import {
  enqueueRenderRequestSchema,
  renderStatusResponseSchema,
  webhookEventSchema,
} from '../schemas.js';

const API = 'https://farm.test';
const auth = {apiUrl: API, apiKey: 'dk_test_secret'};
const response = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {status, headers: {'content-type': 'application/json'}});

afterEach(() => vi.restoreAllMocks());

const base = {
  bundleSha256: 'a'.repeat(64),
  compositionWidth: 1920,
  compositionHeight: 1080,
  fps: 30,
  durationFrames: 300,
};

describe('enqueueRenderRequestSchema still directive (FARM-STILL)', () => {
  it('accepts a still inside the composition and keeps the video fields', () => {
    const parsed = enqueueRenderRequestSchema.parse({
      ...base,
      still: {frame: 12, format: 'png'},
    });
    expect(parsed.still).toEqual({frame: 12, format: 'png'});
    expect(parsed.codec).toBe('h264');
    expect(parsed.durationFrames).toBe(300);
  });

  it('a request WITHOUT still parses with the key ABSENT — byte-identical to the pre-still shape', () => {
    const parsed = enqueueRenderRequestSchema.parse({...base});
    expect(parsed).not.toHaveProperty('still');
  });

  it.each([
    ['frame == durationFrames', {frame: 300, format: 'png'}],
    ['frame past durationFrames', {frame: 301, format: 'png'}],
  ])('refuses %s at parse', (_name, still) => {
    expect(enqueueRenderRequestSchema.safeParse({...base, still}).success).toBe(false);
  });

  it('refuses unknown formats (lossless png only)', () => {
    expect(
      enqueueRenderRequestSchema.safeParse({...base, still: {frame: 5, format: 'jpeg'}}).success,
    ).toBe(false);
  });

  it('enqueueRender sends the directive on the wire', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(
      response({renderId: 'job-still', status: 'pending', taskId: 'task-1', creditsReserved: 5}, 202),
    );
    await enqueueRender({...auth, ...base, still: {frame: 7, format: 'png'}});
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe(`${API}/api/v1/renders`);
    const body = JSON.parse(String(init.body));
    expect(body.still).toEqual({frame: 7, format: 'png'});
  });
});

describe('renderStillOnFarm (FARM-STILL)', () => {
  const stillOptions = {
    ...auth,
    ...base,
    inputProps: {captureRenderer: 'farm'},
    frame: 12,
    pollIntervalMs: 0,
  };

  const completeBody = {
    renderId: 'job-still-1',
    status: 'complete',
    progress: 1,
    outputUrl: 'https://cdn.test/still-f12.png?sig=1',
    creditsReserved: 5,
    creditsSettled: 0,
    error: null,
    createdAt: null,
    completedAt: '2026-09-10T10:00:00.000Z',
    verification: 'pending',
    outputSizeInBytes: 24601,
  };

  it('enqueues with the still directive and returns url + measured size + renderId + frame', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({renderId: 'job-still-1', status: 'pending', taskId: 'render-1', creditsReserved: 5}, 202))
      .mockResolvedValueOnce(response(completeBody));
    const result = await renderStillOnFarm(stillOptions);
    expect(result).toEqual({
      url: 'https://cdn.test/still-f12.png?sig=1',
      sizeInBytes: 24601,
      renderId: 'job-still-1',
      frame: 12,
      verification: 'pending',
      creditsSettled: 0,
    });
    const [, enqueueInit] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    const body = JSON.parse(String(enqueueInit.body));
    expect(body.still).toEqual({frame: 12, format: 'png'});
    expect(body.kind).toBe('gpu'); // WebGPU-capture default, selection only
    // A completed render is never canceled behind the caller's back.
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it('a complete response WITHOUT a measured size is a named client error — never a guessed figure', async () => {
    const {outputSizeInBytes: _omitted, ...legacyComplete} = completeBody;
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({renderId: 'job-still-1', status: 'pending', taskId: 'render-1', creditsReserved: 5}, 202))
      .mockResolvedValueOnce(response(legacyComplete));
    const error = await renderStillOnFarm(stillOptions).then(
      () => 'resolved',
      (e: unknown) => e,
    );
    expect(error).toBeInstanceOf(FarmApiError);
    const farmError = error as FarmApiError;
    expect(farmError.kind).toBe('client');
    expect(farmError.code).toBe('OUTPUT_SIZE_UNAVAILABLE');
  });

  it('chromiumOptions.gl other than angle is refused BEFORE any network call', async () => {
    const fetchSpy = vi.spyOn(globalThis, 'fetch');
    for (const gl of ['swangle', 'swiftshader'] as const) {
      const error = await renderStillOnFarm({...stillOptions, chromiumOptions: {gl}}).then(
        () => 'resolved',
        (e: unknown) => e,
      );
      expect(error).toBeInstanceOf(FarmApiError);
      expect((error as FarmApiError).code).toBe('CHROMIUM_GL_UNSUPPORTED');
    }
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("chromiumOptions.gl: 'angle' (the farm constant) is accepted", async () => {
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({renderId: 'job-still-1', status: 'pending', taskId: 'render-1', creditsReserved: 5}, 202))
      .mockResolvedValueOnce(response(completeBody));
    await expect(
      renderStillOnFarm({...stillOptions, chromiumOptions: {gl: 'angle'}}),
    ).resolves.toMatchObject({frame: 12});
  });

  it('still.frame >= durationFrames is refused client-side before any network call', async () => {
    // The shared enqueue schema's cross-field refine throws a plain ZodError
    // (enqueueRender parses before any fetch) — the same refusal the dispatch
    // front door applies.
    const fetchSpy = vi.spyOn(globalThis, 'fetch');
    await expect(renderStillOnFarm({...stillOptions, frame: 300})).rejects.toThrow(
      /still\.frame must be < durationFrames/,
    );
    await expect(renderStillOnFarm({...stillOptions, frame: 999})).rejects.toThrow(
      /still\.frame must be < durationFrames/,
    );
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});

describe('status + webhook schemas stay honest for stills', () => {
  it('complete status parses with outputSizeInBytes PRESENT and ABSENT (deploy window)', () => {
    const present = renderStatusResponseSchema.safeParse({
      renderId: 'j', status: 'complete', progress: 1,
      outputUrl: 'https://cdn.test/s.png', creditsReserved: 5, creditsSettled: null,
      error: null, createdAt: null, completedAt: null, verification: 'pending',
      outputSizeInBytes: 42,
    });
    expect(present.success).toBe(true);
    const absent = renderStatusResponseSchema.safeParse({
      renderId: 'j', status: 'complete', progress: 1,
      outputUrl: 'https://cdn.test/s.png', creditsReserved: 5, creditsSettled: null,
      error: null, createdAt: null, completedAt: null, verification: 'pending',
    });
    expect(absent.success).toBe(true);
  });

  it('webhook composition.codec is nullable (a still has no video codec)', () => {
    const parsed = webhookEventSchema.parse({
      event: 'render.complete',
      renderId: 'j',
      status: 'complete',
      outputUrl: null,
      error: null,
      creditsReserved: 5,
      creditsSettled: 0,
      verification: 'pending',
      composition: {
        width: 1920, height: 1080, fps: 30, durationFrames: 300,
        codec: null,
      },
      ts: '2026-09-10T10:00:00.000Z',
    });
    expect(parsed.composition.codec).toBeNull();
  });
});
