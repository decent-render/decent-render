#!/usr/bin/env bun
/**
 * Subprocess fixture: runRunner with whatever arrives on stdin. Used to pin the
 * shipped payload's failure contract — a single error frame on stdout, exit 1.
 */
import {runRunner} from '../../index.js';

await runRunner({
	selectComposition: async () => ({durationInFrames: 1}),
	renderMedia: async () => undefined,
});
