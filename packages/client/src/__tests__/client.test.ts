import {afterEach, describe, expect, it, vi} from 'vitest';
import {createHmac} from 'node:crypto';
import {mkdtemp, mkdir, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';

vi.mock('@remotion/bundler', () => ({bundle: vi.fn()}));
import {bundle} from '@remotion/bundler';
import {
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
    const signature = createHmac('sha256', secret).update(`${timestamp}.${body}`).digest('hex');
    expect(verifyWebhookSignature({body, timestamp, signature, secret})).toBe(true);
    expect(verifyWebhookSignature({body, timestamp, signature: `${signature.slice(0, -1)}0`, secret})).toBe(false);
  });

  it('bundles, creates a tar.gz, uploads it, and finalizes registration', async () => {
    const dir = await mkdtemp(path.join(tmpdir(), 'decent-client-test-'));
    await mkdir(path.join(dir, 'assets'));
    await writeFile(path.join(dir, 'index.html'), '<html>render</html>');
    await writeFile(path.join(dir, 'assets', 'app.js'), 'console.log("render")');
    vi.mocked(bundle).mockResolvedValue(dir);
    let uploadedSha = '';
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockImplementationOnce(async (_url, init) => {
        const request = JSON.parse(String(init?.body));
        uploadedSha = request.sha256;
        return response({sha256: uploadedSha, uploadUrl: 'https://r2.test/upload', expiresAt: '2026-07-12T11:00:00.000Z', alreadyRegistered: false}, 201);
      })
      .mockResolvedValueOnce(new Response(null, {status: 200}))
      .mockImplementationOnce(async (_url, init) => {
        const request = JSON.parse(String(init?.body));
        return response({sha256: uploadedSha, remotionVersion: request.remotionVersion, registered: true}, 201);
      });

    const result = await bundleAndUpload({...auth, entryPoint: '/project/remotion.ts', remotionVersion: '4.0.349'});
    expect(result.sha256).toMatch(/^[0-9a-f]{64}$/);
    expect(result.remotionVersion).toBe('4.0.349');
    expect(fetchMock).toHaveBeenCalledTimes(3);
    expect(fetchMock.mock.calls[1]?.[1]?.method).toBe('PUT');
  });
});
