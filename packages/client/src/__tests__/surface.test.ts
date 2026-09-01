import {describe, expect, it} from 'vitest';
import {readFileSync} from 'node:fs';
import * as clientRoot from '../index.js';
import {enqueueRenderRequestSchema} from '../schemas.js';

/**
 * The package's entire VALUE export surface. Types (`RenderStatusResponse`,
 * `EnqueueRenderRequest`, …) are compile-time only and re-exported
 * explicitly; the zod schemas live behind the `@decent-render/client/schemas`
 * subpath. This allow-list is the guard against a bare `export *` creeping
 * back into src/index.ts and leaking the whole schema module through the
 * root entry point (D-15 / U-16).
 */
const PUBLIC_VALUE_EXPORTS = [
  'FarmApiError',
  'bundleAndUpload',
  'cancelRender',
  'createWebhook',
  'deleteWebhook',
  'enqueueRender',
  'getBalance',
  'getLatestBundle',
  'getRenderProgress',
  'getVersions',
  'getWorkerAvailability',
  'isFarmApiError',
  'listWebhooks',
  'renderMediaOnFarm',
  'updateWebhook',
  'verifyWebhookSignature',
].sort();

describe('public export surface (D-15 / U-16)', () => {
  it('exports exactly the documented value surface — no schema leakage via export *', () => {
    expect(Object.keys(clientRoot).sort()).toEqual(PUBLIC_VALUE_EXPORTS);
  });

  it('still exposes the zod schemas through the ./schemas subpath', () => {
    const pkg = JSON.parse(readFileSync(new URL('../../package.json', import.meta.url), 'utf8'));
    expect(pkg.exports['./schemas']).toEqual({
      types: './dist/schemas.d.ts',
      default: './dist/schemas.js',
    });
    // The subpath module itself keeps the request schema (spot check).
    expect(enqueueRenderRequestSchema.parse({
      bundleSha256: 'a'.repeat(64),
      compositionWidth: 1,
      compositionHeight: 1,
      fps: 30,
      durationFrames: 1,
    })).toMatchObject({codec: 'h264', tier: 'cloud', kind: 'standard'});
  });
});
