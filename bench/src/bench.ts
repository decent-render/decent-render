/**
 * decent bench (Phase 0 harness).
 *
 * Runs the REAL compiled runner the way supervisor-core does — a bun --compile
 * binary spawned with cwd set to a fresh temp workdir — and records what an
 * operator node would actually experience:
 *
 *   • cold-start silent window   (spawn → first stdout frame)  — the interval
 *     that must stay under SILENCE_TIMEOUT (runner.rs:18)
 *   • inter-progress gaps        (max gap sets the real liveness interval)
 *   • whether Chrome is downloaded into the workdir and how large it gets
 *   • peak RSS across the whole process tree (runner + Chrome children)
 *   • wall time and effective fps
 *
 * Usage:
 *   bun src/bench.ts --composition=Standard --frames=150 --concurrency=1,2,4
 */
import {createHash} from 'node:crypto';
import {existsSync, mkdtempSync, readFileSync, rmSync, statSync} from 'node:fs';
import {readdir, stat} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {spawn, spawnSync} from 'node:child_process';
import {serve} from './serve.ts';

const root = path.resolve(import.meta.dir, '..');
const benchDir = path.join(root, '.bench');
const runnerBin = path.join(benchDir, 'decent-render-runner');
const archive = path.join(benchDir, 'bundle.tar.gz');

const flag = (name: string, fallback: string): string => {
	const hit = process.argv.find((a) => a.startsWith(`--${name}=`));
	return hit ? hit.slice(name.length + 3) : fallback;
};

const composition = flag('composition', 'Standard');
const frames = Number(flag('frames', '150'));
const width = Number(flag('width', '1920'));
const height = Number(flag('height', '1080'));
const concurrencies = flag('concurrency', '1').split(',').map(Number);
const keepBundleCache = process.argv.includes('--keep-bundle-cache');
// --save-chrome:  keep the downloaded Chrome after a run (one-time seed)
// --warm-chrome:  clone the seeded Chrome into the workdir before spawning.
//                 This simulates decision 2.2 (Chrome shipped in the payload),
//                 so throughput numbers reflect the post-fix state instead of
//                 measuring download bandwidth.
const saveChrome = process.argv.includes('--save-chrome');
const warmChrome = process.argv.includes('--warm-chrome');
const chromeSeed = path.join(benchDir, 'chrome-seed');

if (!existsSync(runnerBin)) throw new Error(`missing runner binary — run: bun run build:runner`);
if (!existsSync(archive)) throw new Error(`missing bundle — run: bun run bundle`);

const bundleSha256 = createHash('sha256').update(readFileSync(archive)).digest('hex');

/** Recursive directory size, tolerant of files vanishing mid-walk. */
async function dirSize(dir: string): Promise<number> {
	let total = 0;
	let entries: string[];
	try {
		entries = await readdir(dir);
	} catch {
		return 0;
	}
	for (const entry of entries) {
		const full = path.join(dir, entry);
		try {
			const s = await stat(full);
			total += s.isDirectory() ? await dirSize(full) : s.size;
		} catch {
			/* raced with purge */
		}
	}
	return total;
}

/** Peak RSS in bytes across a pid and all descendants, via ps. */
function treeRssBytes(rootPid: number): number {
	const ps = spawnSync('ps', ['-eo', 'pid=,ppid=,rss='], {encoding: 'utf8'});
	if (ps.status !== 0) return 0;
	const rows = ps.stdout
		.trim()
		.split('\n')
		.map((line) => line.trim().split(/\s+/).map(Number))
		.filter((r) => r.length === 3 && r.every((n) => Number.isFinite(n)));
	const children = new Map<number, number[]>();
	const rss = new Map<number, number>();
	for (const [pid, ppid, kb] of rows) {
		rss.set(pid, kb * 1024);
		children.set(ppid, [...(children.get(ppid) ?? []), pid]);
	}
	let total = 0;
	const stack = [rootPid];
	const seen = new Set<number>();
	while (stack.length) {
		const pid = stack.pop()!;
		if (seen.has(pid)) continue;
		seen.add(pid);
		total += rss.get(pid) ?? 0;
		stack.push(...(children.get(pid) ?? []));
	}
	return total;
}

type Sample = {
	concurrency: number;
	ok: boolean;
	error?: string;
	coldStartMs: number | null;
	maxProgressGapMs: number | null;
	progressGapsMs: number[];
	wallMs: number;
	fps: number | null;
	peakRssBytes: number;
	chromeInWorkdirBytes: number;
	outputBytes: number | null;
	leakedStdoutLines: string[];
};

async function runOnce(concurrency: number): Promise<Sample> {
	// Fresh workdir per run, exactly like WorkDir::new in supervisor-core.
	const workdir = mkdtempSync(path.join(os.tmpdir(), `job-bench-`));
	const outputPath = path.join(benchDir, `out-${composition}-c${concurrency}.mp4`);
	const server = serve({
		archivePath: archive,
		compositionId: composition,
		inputProps: {durationInFrames: frames, width, height},
		outputPath,
	});
	const base = `http://127.0.0.1:${server.port}`;

	const jobAssign = {
		type: 'jobAssign',
		tenant: 'bench',
		jobId: `bench-${composition}-c${concurrency}`,
		kind: composition === 'WebGPU' ? 'gpu' : 'standard',
		durationFrames: frames,
		fps: 30,
		codec: 'h264',
		bundleSha256,
		bundleGetUrl: `${base}/bundle.tar.gz`,
		payloadSha256: '0'.repeat(64),
		payloadGetUrl: `${base}/unused`,
		inputPropsGetUrl: `${base}/props.json`,
		assetGetUrls: [],
		outputPutUrl: `${base}/output`,
		outputKey: 'bench/out.mp4',
		purgeAfter: true,
	};

	if (warmChrome) {
		if (!existsSync(chromeSeed)) throw new Error('no chrome seed — run once with --save-chrome first');
		// APFS clonefile: near-instant, no 1GB copy.
		const clone = spawnSync('cp', ['-Rc', chromeSeed, path.join(workdir, '.remotion')]);
		if (clone.status !== 0) spawnSync('cp', ['-R', chromeSeed, path.join(workdir, '.remotion')]);
	}

	const startedAt = performance.now();
	let firstFrameAt: number | null = null;
	const progressAt: number[] = [];
	let peakRss = 0;
	let chromePeak = 0;
	let error: string | undefined;
	let doneMetrics: {wallTimeMs?: number} | null = null;
	const leaked: string[] = [];

	const child = spawn(runnerBin, [], {
		cwd: workdir,
		stdio: ['pipe', 'pipe', 'pipe'],
		env: {...process.env, DECENT_BENCH_CONCURRENCY: String(concurrency)},
	});

	const sampler = setInterval(async () => {
		if (child.pid) peakRss = Math.max(peakRss, treeRssBytes(child.pid));
		chromePeak = Math.max(chromePeak, await dirSize(path.join(workdir, '.remotion')));
	}, 500);

	child.stdin.write(JSON.stringify(jobAssign));
	child.stdin.end();

	let stdoutBuf = '';
	child.stdout.on('data', (chunk: Buffer) => {
		stdoutBuf += chunk.toString();
		let nl: number;
		while ((nl = stdoutBuf.indexOf('\n')) !== -1) {
			const line = stdoutBuf.slice(0, nl).trim();
			stdoutBuf = stdoutBuf.slice(nl + 1);
			if (!line) continue;
			const at = performance.now();
			firstFrameAt ??= at;
			try {
				const event = JSON.parse(line) as {type: string; message?: string; wallTimeMs?: number};
				if (event.type === 'progress') progressAt.push(at);
				if (event.type === 'error') error = event.message;
				if (event.type === 'done') doneMetrics = event;
			} catch {
				// Non-NDJSON on stdout: the supervisor logs and ignores these
				// (runner.rs:212), but they DO reset its silence timer. Recorded
				// because anything here is a protocol-stream leak.
				leaked.push(line.slice(0, 200));
			}
		}
	});
	// stderr carries Remotion's own logs, incl. "Downloading Chrome…"
	let stderr = '';
	child.stderr.on('data', (c: Buffer) => {
		stderr += c.toString();
	});

	const code: number = await new Promise((resolve) => child.on('close', resolve));
	clearInterval(sampler);
	const wallMs = performance.now() - startedAt;

	const gaps: number[] = [];
	const marks = [firstFrameAt ?? startedAt, ...progressAt];
	for (let i = 1; i < marks.length; i++) gaps.push(marks[i] - marks[i - 1]);

	const chromeDir = path.join(workdir, '.remotion');
	const chromeFinal = existsSync(chromeDir) ? await dirSize(chromeDir) : 0;
	if (saveChrome && existsSync(chromeDir) && !existsSync(chromeSeed)) {
		spawnSync('cp', ['-Rc', chromeDir, chromeSeed]);
		console.error(`  seeded chrome → ${chromeSeed}`);
	}
	const outputBytes = server.outputBytes();
	server.stop();
	// The supervisor purges the workdir here; do the same so runs are independent.
	rmSync(workdir, {recursive: true, force: true});
	if (!keepBundleCache) rmSync(path.join(os.homedir(), '.decent-worker/bundles', bundleSha256), {recursive: true, force: true});

	if (code !== 0 && !error) error = `runner exited ${code}: ${stderr.slice(-400)}`;

	return {
		concurrency,
		ok: code === 0,
		error,
		coldStartMs: firstFrameAt === null ? null : firstFrameAt - startedAt,
		maxProgressGapMs: gaps.length ? Math.max(...gaps) : null,
		progressGapsMs: gaps.map((g) => Math.round(g)),
		wallMs,
		fps: doneMetrics ? frames / (wallMs / 1000) : null,
		peakRssBytes: peakRss,
		chromeInWorkdirBytes: Math.max(chromePeak, chromeFinal),
		outputBytes,
		leakedStdoutLines: leaked,
	};
}

const gb = (n: number) => `${(n / 1024 ** 3).toFixed(2)}GB`;
const mb = (n: number) => `${(n / 1024 ** 2).toFixed(0)}MB`;

console.error(`node: ${os.cpus()[0]?.model ?? process.arch} · ${Math.round(os.totalmem() / 1024 ** 3)}GB · ${os.cpus().length} cores`);
console.error(`bench: ${composition} ${width}x${height} ${frames}f · concurrency ${concurrencies.join(',')}\n`);

const results: Sample[] = [];
for (const c of concurrencies) {
	console.error(`— running concurrency=${c}…`);
	const sample = await runOnce(c);
	results.push(sample);
	console.error(
		sample.ok
			? `  ok  wall ${(sample.wallMs / 1000).toFixed(1)}s · cold-start ${((sample.coldStartMs ?? 0) / 1000).toFixed(1)}s · max gap ${((sample.maxProgressGapMs ?? 0) / 1000).toFixed(1)}s · peak ${gb(sample.peakRssBytes)} · chrome-in-workdir ${mb(sample.chromeInWorkdirBytes)}`
			: `  FAIL ${sample.error}`,
	);
}

console.log(
	JSON.stringify(
		{
			node: {chip: os.cpus()[0]?.model, ramGb: Math.round(os.totalmem() / 1024 ** 3), cores: os.cpus().length},
			job: {composition, frames, width, height},
			results,
		},
		null,
		2,
	),
);
