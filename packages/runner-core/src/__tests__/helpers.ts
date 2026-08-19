import type {JobAssignMessage} from '@decent-render/protocol';
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {mkdtempSync, readFileSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import path from 'node:path';

/**
 * Build a real gzipped tar of a minimal Remotion bundle (the runner only
 * requires `index.html` to exist for the cache-hit check), plus its sha256 —
 * exactly the artifact shape dispatch presigns as `bundleGetUrl`.
 */
export function makeBundleArchive(marker = 'decent-render test bundle'): {bytes: Buffer; sha256: string} {
	const dir = mkdtempSync(path.join(tmpdir(), 'runner-core-bundle-'));
	writeFileSync(path.join(dir, 'index.html'), `<!doctype html><title>${marker}</title>`);
	const archive = path.join(dir, 'bundle.tar.gz');
	const tarred = spawnSync('tar', ['-czf', archive, '-C', dir, 'index.html']);
	if (tarred.status !== 0) throw new Error(`fixture tar failed: ${tarred.stderr.toString()}`);
	const bytes = readFileSync(archive);
	return {bytes, sha256: createHash('sha256').update(bytes).digest('hex')};
}

export const BUNDLE_URL = 'https://r2.test/render-bundles/bundle.tar.gz?sig=1';
export const PROPS_URL = 'https://r2.test/renders/t1/input-props.json?sig=2';
export const OUTPUT_PUT_URL = 'https://r2.test/renders/t1/out?sig=4';

/** A wire-valid jobAssign frame (same field set as protocol `fixtures/v2.json`). */
export function jobAssign(overrides: Partial<JobAssignMessage> = {}): JobAssignMessage {
	return {
		type: 'jobAssign',
		tenant: 'driffs',
		jobId: 'job-test-1',
		attempt: 1,
		kind: 'gpu',
		durationFrames: 24,
		fps: 30,
		codec: 'h264',
		bundleSha256: 'a'.repeat(64),
		bundleGetUrl: BUNDLE_URL,
		payloadSha256: 'b'.repeat(64),
		payloadGetUrl: 'https://r2.test/render-payloads/payload.tar.gz?sig=3',
		inputPropsGetUrl: PROPS_URL,
		assetGetUrls: [],
		outputPutUrl: OUTPUT_PUT_URL,
		outputKey: 'renders/t1/out.mp4',
		purgeAfter: true,
		...overrides,
	};
}
