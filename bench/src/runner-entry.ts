/**
 * A real runner entry, identical in shape to farm-web's apps/runner-<version>.
 * The `@remotion/renderer` import MUST stay here so `bun build --compile`
 * embeds THIS package's pinned version — see runner-core/renderer-api.ts.
 *
 * The bench compiles and runs this exactly as the supervisor runs a payload
 * (compiled binary, cwd = a fresh temp workdir), so measurements reflect the
 * real operator path rather than a dev-server approximation.
 *
 * The one bench-only deviation: renderMedia is wrapped so the harness can sweep
 * concurrency via DECENT_BENCH_CONCURRENCY. runner-core still hardcodes 1 — we
 * measure the current code path rather than modifying the thing under test.
 * (Decision 2.3 will later carry maxConcurrency in the stdin envelope.)
 */
import {renderMedia, selectComposition} from '@remotion/renderer';
import {runRunner} from '@decent-render/runner-core';

const override = Number(process.env.DECENT_BENCH_CONCURRENCY ?? '') || null;

await runRunner({
	selectComposition,
	renderMedia: (options) =>
		renderMedia(override === null ? options : {...options, concurrency: override}),
});
