import {afterEach, describe, expect, it, vi} from 'vitest';
import {createHash, createHmac} from 'node:crypto';
import {mkdtemp, mkdir, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';

vi.mock('@remotion/bundler', () => ({bundle: vi.fn()}));
import {bundle} from '@remotion/bundler';
import {
  FarmApiError,
  bundleAndUpload,
  cancelRender,
  getBalance,
  getRenderProgress,
  getLatestBundle,
  getVersions,
  getWorkerAvailability,
  renderMediaOnFarm,
  verifyWebhookSignature,
} from '../index.js';

const API = 'https://farm.test';
const auth = {apiUrl: API, apiKey: 'dk_test_secret'};
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), {status, headers: {'content-type': 'application/json'}});

afterEach(() => vi.restoreAllMocks());

/** URLs of every POST to a `/cancel` endpoint seen by the fetch mock. */
const cancelCalls = (fetchMock: {mock: {calls: unknown[][]}}) =>
  fetchMock.mock.calls
    .filter(([url, init]) => String(url).endsWith('/cancel') && (init as RequestInit | undefined)?.method === 'POST')
    .map(([url]) => String(url));

describe('farm client', () => {
  it('polls renderMediaOnFarm until complete and resolves a playable result', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({renderId: 'job-1', status: 'pending', taskId: 'render-1', creditsReserved: 5}, 202))
      .mockResolvedValueOnce(response({renderId: 'job-1', status: 'rendering', progress: 0.5, outputUrl: null, creditsReserved: 5, creditsSettled: null, error: null, createdAt: null, completedAt: null, verification: 'pending'}))
      .mockResolvedValueOnce(response({renderId: 'job-1', status: 'complete', progress: 1, outputUrl: 'https://cdn.test/video.mp4?sig=1', creditsReserved: 5, creditsSettled: 5, error: null, createdAt: null, completedAt: '2026-07-12T10:00:00.000Z', verification: 'passed'}));

    const result = await renderMediaOnFarm({
      ...auth,
      bundleSha256: 'a'.repeat(64), inputProps: {}, compositionId: 'Main',
      compositionWidth: 1080, compositionHeight: 1920, fps: 30,
      durationFrames: 90, codec: 'h264', pollIntervalMs: 0,
    });
    expect(result).toEqual({outputUrl: 'https://cdn.test/video.mp4?sig=1', renderId: 'job-1', creditsSettled: 5, verification: 'passed'});
    expect(fetchMock).toHaveBeenCalledTimes(3);
    // A-5: a render that completed is never canceled.
    expect(cancelCalls(fetchMock)).toEqual([]);
  });

  // ── A-5: renderMediaOnFarm must cancel what it abandons ─────────────────
  //
  // A render the caller stopped waiting for keeps rendering (and billing) on
  // the farm unless someone cancels it. Both abandonment paths — the internal
  // timeout and an external AbortSignal — must POST the cancel endpoint for
  // that render exactly once before surfacing the error.

  const renderRequest = {
    bundleSha256: 'd'.repeat(64), compositionId: 'Main', inputProps: {},
    compositionWidth: 1, compositionHeight: 1, fps: 30, durationFrames: 1, codec: 'h264' as const,
  };
  const rendering = (renderId: string) => ({renderId, status: 'rendering', progress: 0.5, outputUrl: null, creditsReserved: 5, creditsSettled: null, error: null, createdAt: null, completedAt: null, verification: 'pending'});

  /** Route by URL so the test is about ORDER OF EVENTS, not a fragile mockOnce chain. */
  function farmThatNeverFinishes(renderId: string) {
    return vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
      const url = String(input);
      if (url.endsWith('/cancel')) return response({renderId, status: 'canceled'});
      if (url.endsWith('/api/v1/renders') && init?.method === 'POST') return response({renderId, status: 'pending', taskId: `task-${renderId}`, creditsReserved: 5}, 202);
      return response(rendering(renderId));
    });
  }

  it('cancels the render on the internal timeout — exactly one POST /cancel — then throws 408', async () => {
    vi.useFakeTimers();
    try {
      const fetchMock = farmThatNeverFinishes('job-timeout');
      const pending = renderMediaOnFarm({...auth, ...renderRequest, pollIntervalMs: 1000, timeoutMs: 2500});
      const outcome = pending.then(() => 'resolved', (e: unknown) => e);
      // Two polls at t=1000 and t=2000 land before the deadline; the third
      // (t=3000) is past it.
      await vi.advanceTimersByTimeAsync(5000);
      const error = await outcome;
      expect(error).toBeInstanceOf(FarmApiError);
      expect((error as FarmApiError).status).toBe(408);
      expect((error as FarmApiError).code).toBe('RENDER_TIMEOUT');
      expect(cancelCalls(fetchMock)).toEqual([`${API}/api/v1/renders/job-timeout/cancel`]);
      // The cancel is the LAST thing that happens before the throw.
      const last = fetchMock.mock.calls.at(-1)!;
      expect(String(last[0])).toMatch(/\/cancel$/);
      expect(last[1]?.method).toBe('POST');
    } finally {
      vi.useRealTimers();
    }
  });

  it('cancels the render when an external AbortSignal fires — exactly one POST /cancel — then rejects with the abort reason', async () => {
    vi.useFakeTimers();
    try {
      const fetchMock = farmThatNeverFinishes('job-abort');
      const controller = new AbortController();
      const pending = renderMediaOnFarm({...auth, ...renderRequest, pollIntervalMs: 1000, timeoutMs: 60_000, signal: controller.signal});
      const outcome = pending.then(() => 'resolved', (e: unknown) => e);
      // Enqueue + first poll happen at t=0; the loop is now sleeping.
      await vi.advanceTimersByTimeAsync(0);
      const reason = new Error('caller went away');
      controller.abort(reason);
      await vi.advanceTimersByTimeAsync(5000);
      expect(await outcome).toBe(reason);
      expect(cancelCalls(fetchMock)).toEqual([`${API}/api/v1/renders/job-abort/cancel`]);
      // The cancel request must NOT carry the already-aborted signal, or it
      // would abort itself before reaching the farm.
      const cancel = fetchMock.mock.calls.find(([u]) => String(u).endsWith('/cancel'))!;
      expect(cancel[1]?.signal?.aborted ?? false).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it('does not cancel a render that reached a terminal state on its own', async () => {
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({renderId: 'job-term', status: 'pending', taskId: 'render-t', creditsReserved: 5}, 202))
      .mockResolvedValueOnce(response({renderId: 'job-term', status: 'canceled', progress: null, outputUrl: null, creditsReserved: 5, creditsSettled: null, error: null, createdAt: null, completedAt: null, verification: 'pending'}));
    const fetchMock = vi.mocked(fetch);
    await expect(renderMediaOnFarm({...auth, ...renderRequest, pollIntervalMs: 0})).rejects.toMatchObject({code: 'RENDER_CANCELED'});
    expect(cancelCalls(fetchMock)).toEqual([]);
  });

  it('surfaces terminal farm failures', async () => {
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({renderId: 'job-2', status: 'pending', taskId: 'render-2', creditsReserved: 5}, 202))
      .mockResolvedValueOnce(response({renderId: 'job-2', status: 'failed', progress: null, outputUrl: null, creditsReserved: 5, creditsSettled: null, error: 'render crashed', createdAt: null, completedAt: null, verification: 'pending'}));
    await expect(renderMediaOnFarm({...auth, bundleSha256: 'b'.repeat(64), compositionWidth: 1, compositionHeight: 1, fps: 30, durationFrames: 1, codec: 'h264', pollIntervalMs: 0})).rejects.toThrow('render crashed');
  });

  it('parses progress, cancel, and hold-aware balance responses', async () => {
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response({renderId: 'job-3', status: 'rendering', progress: 0.25, outputUrl: null, creditsReserved: 5, creditsSettled: null, error: null, createdAt: null, completedAt: null, verification: 'pending'}))
      .mockResolvedValueOnce(response({renderId: 'job-3', status: 'canceled'}))
      .mockResolvedValueOnce(response({balance: 10, holds: 4, available: 6}));
    expect((await getRenderProgress({...auth, renderId: 'job-3'})).progress).toBe(0.25);
    expect((await cancelRender({...auth, renderId: 'job-3'})).status).toBe('canceled');
    expect(await getBalance(auth)).toEqual({balance: 10, holds: 4, available: 6});
  });

  it('preflights active farm runner versions', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response({
      supportedRemotionVersions: [{remotionVersion: '4.0.487', payloadVersion: 'runner-487'}],
    }));
    expect((await getVersions(auth)).supportedRemotionVersions[0]?.remotionVersion).toBe('4.0.487');
  });

  it('fetches the latest registered bundle', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response({sha256: 'c'.repeat(64)}));
    expect(await getLatestBundle(auth)).toEqual({sha256: 'c'.repeat(64)});
  });

  it('checks operator availability through the scoped farm endpoint', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response({anyConnected: true}));
    expect(await getWorkerAvailability({...auth, operator: 'operator/1'})).toEqual({anyConnected: true});
    expect(String(fetchMock.mock.calls[0]?.[0])).toContain('operator%2F1');
  });

  it('verifies webhook signatures in constant-time compatible hex form', () => {
    const body = '{"event":"render.complete"}';
    const secret = 'whsec_test';
    const timestamp = '1783850400';
    const now = 1783850400;
    const signature = createHmac('sha256', secret).update(`${timestamp}.${body}`).digest('hex');
    expect(verifyWebhookSignature({body, timestamp, signature, secret, now})).toBe(true);
    expect(verifyWebhookSignature({body, timestamp, signature: `${signature.slice(0, -1)}0`, secret, now})).toBe(false);
  });

  it('rejects a delivery outside the replay window, in either direction, and accepts the edge', () => {
    const body = '{"event":"render.complete"}';
    const secret = 'whsec_test';
    const timestamp = '1783850400';
    const signature = createHmac('sha256', secret).update(`${timestamp}.${body}`).digest('hex');
    const ok = (now: number, toleranceSeconds?: number) =>
      verifyWebhookSignature({body, timestamp, signature, secret, now, toleranceSeconds});
    expect(ok(1783850400 + 300)).toBe(true);   // exactly at the edge
    expect(ok(1783850400 - 300)).toBe(true);
    expect(ok(1783850400 + 301)).toBe(false);  // stale
    expect(ok(1783850400 - 301)).toBe(false);  // from the future
    expect(ok(1783850400 + 3000, 3600)).toBe(true); // a wider window is the caller's call
    // A timestamp that is not a unix-seconds integer never verifies, even with a correct HMAC.
    const odd = 'not-a-number';
    const oddSig = createHmac('sha256', secret).update(`${odd}.${body}`).digest('hex');
    expect(verifyWebhookSignature({body, timestamp: odd, signature: oddSig, secret, now: 0})).toBe(false);
  });

  it('defaults to the wall clock: a delivery signed now verifies, one from an hour ago does not', () => {
    const body = '{"event":"render.complete"}';
    const secret = 'whsec_test';
    const fresh = String(Math.floor(Date.now() / 1000));
    const freshSig = createHmac('sha256', secret).update(`${fresh}.${body}`).digest('hex');
    expect(verifyWebhookSignature({body, timestamp: fresh, signature: freshSig, secret})).toBe(true);
    const old = String(Math.floor(Date.now() / 1000) - 3600);
    const oldSig = createHmac('sha256', secret).update(`${old}.${body}`).digest('hex');
    expect(verifyWebhookSignature({body, timestamp: old, signature: oldSig, secret})).toBe(false);
  });

  it('bundles, creates a tar.gz, uploads it, and finalizes registration', async () => {
    const dir = await mkdtemp(path.join(tmpdir(), 'decent-client-test-'));
    await mkdir(path.join(dir, 'assets'));
    await writeFile(path.join(dir, 'index.html'), '<html>render</html>');
    await writeFile(path.join(dir, 'assets', 'app.js'), 'console.log("render")');
    vi.mocked(bundle).mockResolvedValue(dir);
    let uploadedSha = '';
    let putBodySha256 = '';
    let putBodyBytes = -1;
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockImplementationOnce(async (_url, init) => {
        const request = JSON.parse(String(init?.body));
        uploadedSha = request.sha256;
        return response({sha256: uploadedSha, uploadUrl: 'https://r2.test/upload', expiresAt: '2026-07-12T11:00:00.000Z', alreadyRegistered: false}, 201);
      })
      .mockImplementationOnce(async (_url, init) => {
        // C-9: hash the bytes that actually went up the wire. The sha the
        // client REGISTERS must be the sha of the bytes it UPLOADS — that is
        // the content-addressing contract the farm (and every node's sha256
        // gate) relies on.
        const body = init?.body;
        if (!(body instanceof Uint8Array)) throw new Error(`PUT body must be bytes, got ${typeof body}`);
        putBodySha256 = createHash('sha256').update(body).digest('hex');
        putBodyBytes = body.byteLength;
        return new Response(null, {status: 200});
      })
      .mockImplementationOnce(async (_url, init) => {
        const request = JSON.parse(String(init?.body));
        return response({sha256: uploadedSha, remotionVersion: request.remotionVersion, registered: true}, 201);
      });

    const result = await bundleAndUpload({...auth, entryPoint: '/project/remotion.ts', remotionVersion: '4.0.349'});
    expect(result.sha256).toMatch(/^[0-9a-f]{64}$/);
    expect(result.remotionVersion).toBe('4.0.349');
    expect(fetchMock).toHaveBeenCalledTimes(3);
    expect(fetchMock.mock.calls[1]?.[1]?.method).toBe('PUT');
    // Registered sha == uploaded bytes' sha == reported sha; size likewise.
    expect(putBodySha256).toBe(result.sha256);
    expect(uploadedSha).toBe(result.sha256);
    expect(putBodyBytes).toBe(result.sizeBytes);
    // And the uploaded bytes are a real gzip stream (magic 1f 8b).
    const put = fetchMock.mock.calls[1]?.[1]?.body as Uint8Array;
    expect([put[0], put[1]]).toEqual([0x1f, 0x8b]);
  });
});
