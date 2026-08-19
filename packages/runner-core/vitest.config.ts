import {defineConfig} from 'vitest/config';

export default defineConfig({
	test: {
		include: ['src/**/*.test.ts'],
		environment: 'node',
		// The stdout/stderr discipline suite spawns real `bun` subprocesses.
		testTimeout: 30_000,
	},
});
