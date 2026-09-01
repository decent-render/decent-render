import {afterEach, describe, expect, it, vi} from 'vitest';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';

vi.mock('@remotion/bundler', () => ({bundle: vi.fn()}));
import {bundle} from '@remotion/bundler';
import {FarmApiError, bundleAndUpload, getRenderProgress, isFarmApiError, renderMediaOnFarm} from '../index.js';

const API = 'https://farm.test';
const auth = {apiUrl: API, apiKey: 'dk_test_secret'};
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), {status, headers: {'content-type': 'application/json'}});

afterEach(() => vi.restoreAllMocks());

const renderRequest = {
  bundleSha256: 'e'.repeat(64), compositionId: 'Main', inputProps: {},
  compositionWidth: 1, compositionHeight: 1, fps: 30, durationFrames: 1, codec: 'h264' as const,
};
const rendering = (renderId: string) => ({renderId, status: 'rendering', progress: 0.5, outputUrl: null, creditsReserved: 5, creditsSettled: null, error: null, createdAt: null, completedAt: null, verification: 'pending'});

/** Route by URL: enqueue 202s, everything else polls "rendering" forever. */
function farmThatNeverFinishes(renderId: string) {
  return vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
    const url = String(input);
    if (url.endsWith('/cancel')) return response({renderId, status: 'canceled'});
    if (url.endsWith('/api/v1/renders') && init?.method === 'POST') return response({renderId, status: 'pending', taskId: `task-${renderId}`, creditsReserved: 5}, 202);
    return response(rendering(renderId));
  });
}

describe('FarmApiError kinds (D-15 / U-15)', () => {
  it('a real farm HTTP failure is kind http with the real status', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response({error: 'nope', code: 'NOPE'}, 503));
    const error = await getRenderProgress({...auth, renderId: 'job-kind-http'}).then(
      () => 'resolved',
      (e: unknown) => e,
    );
    expect(isFarmApiError(error)).toBe(true);
    const farmError = error as FarmApiError;
    expect(farmError.kind).toBe('http');
    expect(farmError.status).toBe(503);
    expect(farmError.code).toBe('NOPE');
  });

  it('the 408 timeout status is synthetic — kind client', async () => {
    vi.useFakeTimers();
    try {
      farmThatNeverFinishes('job-kind-timeout');
      const pending = renderMediaOnFarm({...auth, ...renderRequest, pollIntervalMs: 1000, timeoutMs: 2500});
      const outcome = pending.then(() => 'resolved', (e: unknown) => e);
      await vi.advanceTimersByTimeAsync(5000);
      const error = await outcome;
      expect(isFarmApiError(error)).toBe(true);
      expect((error as FarmApiError).kind).toBe('client');
      expect((error as FarmApiError).status).toBe(408);
      expect((error as FarmApiError).code).toBe('RENDER_TIMEOUT');
    } finally {
      vi.useRealTimers();
    }
  });

  it('a terminal failed/canceled status arrives inside a 200 poll — its 409 is synthetic, kind client', async () => {
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({renderId: 'job-kind-failed', status: 'pending', taskId: 'render-kf', creditsReserved: 5}, 202))
      .mockResolvedValueOnce(response({renderId: 'job-kind-failed', status: 'failed', progress: null, outputUrl: null, creditsReserved: 5, creditsSettled: null, error: 'render crashed', createdAt: null, completedAt: null, verification: 'pending'}));
    const error = await renderMediaOnFarm({...auth, ...renderRequest, pollIntervalMs: 0}).then(
      () => 'resolved',
      (e: unknown) => e,
    );
    expect(isFarmApiError(error)).toBe(true);
    expect((error as FarmApiError).kind).toBe('client');
    expect((error as FarmApiError).status).toBe(409);
    expect((error as FarmApiError).code).toBe('RENDER_FAILED');
  });

  it('a webhook callback returning a non-terminal status invents 500 — kind client', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response({renderId: 'job-kind-hook', status: 'pending', taskId: 'render-kh', creditsReserved: 5}, 202));
    const error = await renderMediaOnFarm({
      ...auth, ...renderRequest,
      waitForCompletion: async () => rendering('job-kind-hook') as unknown as Parameters<NonNullable<Parameters<typeof renderMediaOnFarm>[0]['waitForCompletion']>>[0],
    }).then(() => 'resolved', (e: unknown) => e);
    expect(isFarmApiError(error)).toBe(true);
    expect((error as FarmApiError).kind).toBe('client');
    expect((error as FarmApiError).status).toBe(500);
  });

  it('a REAL presigned-PUT storage failure keeps kind http', async () => {
    const dir = await mkdtemp(path.join(tmpdir(), 'decent-kind-http-'));
    await writeFile(path.join(dir, 'index.html'), '<html>x</html>');
    vi.mocked(bundle).mockResolvedValue(dir);
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({sha256: 'a'.repeat(64), uploadUrl: 'https://r2.test/upload', expiresAt: null, alreadyRegistered: false}, 201))
      .mockResolvedValueOnce(new Response(null, {status: 500}));
    const error = await bundleAndUpload({...auth, entryPoint: '/p/index.ts', remotionVersion: '4.0.349'}).then(
      () => 'resolved',
      (e: unknown) => e,
    );
    expect(isFarmApiError(error)).toBe(true);
    expect((error as FarmApiError).kind).toBe('http');
    expect((error as FarmApiError).status).toBe(500);
  });

  it('a missing upload URL and a sha mismatch are client-side fabrications — kind client', async () => {
    const dir = await mkdtemp(path.join(tmpdir(), 'decent-kind-client-'));
    await writeFile(path.join(dir, 'index.html'), '<html>x</html>');
    vi.mocked(bundle).mockResolvedValue(dir);

    // No uploadUrl provided though the bundle was not already registered.
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response({sha256: 'a'.repeat(64), uploadUrl: null, expiresAt: null, alreadyRegistered: false}, 201));
    const missingUrl = await bundleAndUpload({...auth, entryPoint: '/p/index.ts', remotionVersion: '4.0.349'}).then(
      () => 'resolved',
      (e: unknown) => e,
    );
    expect(isFarmApiError(missingUrl)).toBe(true);
    expect((missingUrl as FarmApiError).kind).toBe('client');
    expect((missingUrl as FarmApiError).status).toBe(500);

    // The farm registered a different sha — the 500 is invented client-side.
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({sha256: 'a'.repeat(64), uploadUrl: 'https://r2.test/upload', expiresAt: null, alreadyRegistered: false}, 201))
      .mockResolvedValueOnce(new Response(null, {status: 200}))
      .mockResolvedValueOnce(response({sha256: 'b'.repeat(64), remotionVersion: '4.0.349', registered: true}, 201));
    const mismatch = await bundleAndUpload({...auth, entryPoint: '/p/index.ts', remotionVersion: '4.0.349'}).then(
      () => 'resolved',
      (e: unknown) => e,
    );
    expect(isFarmApiError(mismatch)).toBe(true);
    expect((mismatch as FarmApiError).kind).toBe('client');
    expect((mismatch as FarmApiError).status).toBe(500);
  });

  it('isFarmApiError is false for abort reasons and plain errors', async () => {
    vi.useFakeTimers();
    try {
      const fetchMock = farmThatNeverFinishes('job-kind-abort');
      const controller = new AbortController();
      const pending = renderMediaOnFarm({...auth, ...renderRequest, pollIntervalMs: 1000, timeoutMs: 60_000, signal: controller.signal});
      const outcome = pending.then(() => 'resolved', (e: unknown) => e);
      await vi.advanceTimersByTimeAsync(0);
      const reason = new Error('caller went away');
      controller.abort(reason);
      await vi.advanceTimersByTimeAsync(5000);
      expect(await outcome).toBe(reason);
      // The abort reason rides through untouched: not a FarmApiError.
      expect(isFarmApiError(reason)).toBe(false);
      expect(isFarmApiError(new Error('plain'))).toBe(false);
      expect(isFarmApiError('nope')).toBe(false);
      expect(fetchMock).toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});
