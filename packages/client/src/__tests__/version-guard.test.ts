import {afterEach, describe, expect, it, vi} from 'vitest';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';

vi.mock('@remotion/bundler', () => ({bundle: vi.fn()}));
import {bundle} from '@remotion/bundler';
import {FarmApiError, bundleAndUpload} from '../index.js';

const auth = {apiUrl: 'https://farm.test', apiKey: 'dk_test_secret'};

afterEach(() => vi.restoreAllMocks());

describe('remotionVersion is validated before archiving (D-15 / U-16 tail)', () => {
  it.each(['', '4.0', 'v4.0.349', '4.0.349-beta', 'latest'])(
    'rejects %j before any fs read or network call',
    async (bad) => {
      const dir = await mkdtemp(path.join(tmpdir(), 'decent-version-guard-'));
      await writeFile(path.join(dir, 'index.html'), '<html>x</html>');
      vi.mocked(bundle).mockResolvedValue(dir);
      const fetchSpy = vi.spyOn(globalThis, 'fetch');

      const error = await bundleAndUpload({...auth, entryPoint: '/p/index.ts', remotionVersion: bad}).then(
        () => 'resolved',
        (e: unknown) => e,
      );
      expect(error).toBeInstanceOf(FarmApiError);
      const farmError = error as FarmApiError;
      // The bad version is the CALLER's problem, not the farm's: kind client.
      expect(farmError.kind).toBe('client');
      expect(farmError.status).toBe(400);
      expect(farmError.message).toContain('remotionVersion');
      // The expensive work never started.
      expect(bundle).not.toHaveBeenCalled();
      expect(fetchSpy).not.toHaveBeenCalled();
    },
  );

  it('accepts a full semver release and proceeds to archive', async () => {
    const dir = await mkdtemp(path.join(tmpdir(), 'decent-version-guard-ok-'));
    await writeFile(path.join(dir, 'index.html'), '<html>x</html>');
    vi.mocked(bundle).mockResolvedValue(dir);
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(
      new Response(JSON.stringify({sha256: 'a'.repeat(64), uploadUrl: null, expiresAt: null, alreadyRegistered: true}), {status: 201, headers: {'content-type': 'application/json'}}),
    );
    const result = await bundleAndUpload({...auth, entryPoint: '/p/index.ts', remotionVersion: '4.0.349'});
    expect(result.alreadyRegistered).toBe(true);
    expect(bundle).toHaveBeenCalledTimes(1);
  });
});
