import {describe, it, expect, vi, beforeEach} from 'vitest';
import {render, screen, waitFor, fireEvent, act} from '@testing-library/react';
import React from 'react';

// ── Mock @tauri-apps/api ───────────────────────────────────────────────────

// The invoke mock — tests can inspect call args and control return values.
const invokeMock = vi.fn();

// The listen mock — returns an unlisten function. Tests capture the handler
// to simulate backend events (status-update, log-line).
type EventHandler = (event: {payload: unknown}) => void;
let statusHandler: EventHandler | null = null;
let logHandler: EventHandler | null = null;

vi.mock('@tauri-apps/api/core', () => ({
	invoke: (cmd: string, args?: Record<string, unknown>) => invokeMock(cmd, args),
}));

// The shell plugin's `open` — how "Connect this Mac" reaches the system
// browser (A-3). Mocked so the test can assert the exact URL.
const shellOpenMock = vi.fn(async (_url: string) => undefined);
vi.mock('@tauri-apps/plugin-shell', () => ({
	open: (url: string) => shellOpenMock(url),
}));

vi.mock('@tauri-apps/api/event', () => ({
	listen: vi.fn(async (event: string, handler: EventHandler) => {
		if (event === 'status-update') statusHandler = handler;
		if (event === 'log-line') logHandler = handler;
		return () => {
			if (event === 'status-update') statusHandler = null;
			if (event === 'log-line') logHandler = null;
		};
	}),
}));

// ── Import App AFTER mocks are set up ──────────────────────────────────────

import App from './App';

describe('App', () => {
	beforeEach(() => {
		invokeMock.mockReset();
		shellOpenMock.mockClear();
		statusHandler = null;
		logHandler = null;

		// Default mock responses for initial load.
		invokeMock.mockImplementation(async (cmd: string) => {
			switch (cmd) {
				case 'get_config':
					return {
						dispatchUrl: 'ws://localhost:8790/ws',
						workdirRoot: null,
						allowRealJobsDefault: false,
					};
				case 'get_status':
					return {
						connection: 'disconnected',
						dispatchUrl: null,
						nodeIdentity: null,
						currentJob: null,
						jobsCompleted: 0,
						jobsFailed: 0,
						jobsCanceled: 0,
						lastError: null,
						allowRealJobs: false,
					};
				case 'get_allow_real_jobs':
					return false;
				case 'get_token':
					return '';
				case 'start_connection':
					return null;
				case 'stop_connection':
					return null;
				case 'set_allow_real_jobs':
					return null;
				case 'save_app_config':
					return null;
				case 'save_token_cmd':
					return null;
				default:
					return null;
			}
		});
	});

	it('renders connection badge as DISCONNECTED on initial load', async () => {
		render(<App />);
		await waitFor(() => {
			expect(screen.getByText('DISCONNECTED')).toBeInTheDocument();
		});
	});

	it('shows REGISTERED when status-update event fires', async () => {
		render(<App />);
		await waitFor(() => {
			expect(statusHandler).not.toBeNull();
		});

		act(() => {
			statusHandler!({
				payload: {
					connection: 'registered',
					dispatchUrl: 'ws://localhost:8790/ws',
					nodeIdentity: {
						chip: 'Apple M4 Max',
						platform: 'company',
						protocolVersion: 2,
						supervisorVersion: 'rust-0.0.1-app',
					},
					currentJob: null,
					jobsCompleted: 0,
					jobsFailed: 0,
					jobsCanceled: 0,
					lastError: null,
					allowRealJobs: false,
				},
			});
		});

		expect(screen.getByText('REGISTERED')).toBeInTheDocument();
		expect(screen.getByText('Apple M4 Max')).toBeInTheDocument();
	});

	it('shows progress bar when a job is active', async () => {
		render(<App />);
		await waitFor(() => {
			expect(statusHandler).not.toBeNull();
		});

		act(() => {
			statusHandler!({
				payload: {
					connection: 'registered',
					dispatchUrl: 'ws://localhost:8790/ws',
					nodeIdentity: null,
					currentJob: {
						id: 'spike-render-test',
						tier: 'gpu',
						progress: 0.5,
						phase: 'rendering',
					},
					jobsCompleted: 0,
					jobsFailed: 0,
					jobsCanceled: 0,
					lastError: null,
					allowRealJobs: true,
				},
			});
		});

		expect(screen.getByText('spike-render-test')).toBeInTheDocument();
		expect(screen.getByText('50%')).toBeInTheDocument();
	});

	it('renders each log line exactly once (dup-logs regression)', async () => {
		render(<App />);
		await waitFor(() => {
			expect(logHandler).not.toBeNull();
		});

		// Simulate two distinct log lines from the backend.
		act(() => {
			logHandler!({
				payload: {
					timestampMs: Date.now(),
					level: 'info',
					message: 'Connected to dispatch',
				},
			});
			logHandler!({
				payload: {
					timestampMs: Date.now(),
					level: 'info',
					message: 'Registered as Apple M4 Max',
				},
			});
		});

		// Each line should appear exactly once.
		const connected = screen.getAllByText('Connected to dispatch');
		expect(connected).toHaveLength(1);
		const registered = screen.getAllByText('Registered as Apple M4 Max');
		expect(registered).toHaveLength(1);
	});

	it('calls start_connection with dispatch URL and token on Start click', async () => {
		render(<App />);
		await waitFor(() => {
			expect(screen.getByText('Start')).toBeInTheDocument();
		});

		// Type a token.
		const tokenInput = screen.getByPlaceholderText('JWT worker token');
		await act(async () => {
			fireEvent.change(tokenInput, {target: {value: 'test-jwt-token'}});
		});

		// Click Start.
		await act(async () => {
			fireEvent.click(screen.getByText('Start'));
		});

		await waitFor(() => {
			expect(invokeMock).toHaveBeenCalledWith('start_connection', {
				dispatchUrl: 'ws://localhost:8790/ws',
				token: 'test-jwt-token',
			});
		});
	});

	// The token-shape gate (shared with `decent login`): when the backend
	// refuses the paste, the operator sees WHY and no connection is attempted.
	it('a refused token paste is shown to the operator and start_connection is never called', async () => {
		const alertSpy = vi.spyOn(window, 'alert').mockImplementation(() => undefined);
		const base = invokeMock.getMockImplementation()!;
		invokeMock.mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
			if (cmd === 'save_token_cmd') throw 'not a worker token: expected three dot-separated parts, got 1';
			return base(cmd, args);
		});
		render(<App />);
		await waitFor(() => {
			expect(screen.getByText('Start')).toBeInTheDocument();
		});
		await act(async () => {
			fireEvent.change(screen.getByPlaceholderText('JWT worker token'), {target: {value: 'paste-your-token-here'}});
		});
		await act(async () => {
			fireEvent.click(screen.getByText('Start'));
		});
		await waitFor(() => {
			expect(alertSpy).toHaveBeenCalledWith('Failed to start: not a worker token: expected three dot-separated parts, got 1');
		});
		expect(invokeMock.mock.calls.some(([cmd]) => cmd === 'start_connection')).toBe(false);
		alertSpy.mockRestore();
	});

	// C-8: the consent toggle is the operator's real-jobs opt-in, and Start
	// must persist exactly what it shows. Without opting in, the IPC carries
	// allowRealJobsDefault: false; after clicking the toggle, true — and the
	// same click flips the live gate via set_allow_real_jobs.
	it('consent toggle drives allowRealJobsDefault on start', async () => {
		render(<App />);
		const tokenInput = await screen.findByPlaceholderText('JWT worker token');
		await act(async () => {
			fireEvent.change(tokenInput, {target: {value: 'test-jwt-token'}});
		});

		// Default: consent OFF → Start must persist false.
		await act(async () => {
			fireEvent.click(screen.getByText('Start'));
		});
		await waitFor(() => {
			expect(invokeMock).toHaveBeenCalledWith('save_app_config', {
				dispatchUrl: 'ws://localhost:8790/ws',
				workdirRoot: null,
				allowRealJobsDefault: false,
			});
		});

		// Opt in: the toggle flips the live gate AND the persisted default.
		const toggle = screen.getByRole('checkbox', {
			name: /accept real render jobs/i,
		});
		await act(async () => {
			fireEvent.click(toggle);
		});
		expect(invokeMock).toHaveBeenCalledWith('set_allow_real_jobs', {
			value: true,
		});

		await act(async () => {
			fireEvent.click(screen.getByText('Start'));
		});
		await waitFor(() => {
			expect(invokeMock).toHaveBeenCalledWith('save_app_config', {
				dispatchUrl: 'ws://localhost:8790/ws',
				workdirRoot: null,
				allowRealJobsDefault: true,
			});
		});
	});

	it('session stats increment when status updates', async () => {
		render(<App />);
		await waitFor(() => {
			expect(statusHandler).not.toBeNull();
		});

		act(() => {
			statusHandler!({
				payload: {
					connection: 'registered',
					dispatchUrl: null,
					nodeIdentity: null,
					currentJob: null,
					jobsCompleted: 3,
					jobsFailed: 1,
					jobsCanceled: 2,
					lastError: null,
					allowRealJobs: false,
				},
			});
		});

		// C-8: each stat is found via its LABEL, not by bare value text (which
		// can collide as the UI grows) and not by row index (which silently
		// keeps passing after a row reorder).
		const statValue = (label: string): string => {
			const stat = screen.getByText(label).closest('.stat');
			expect(stat).not.toBeNull();
			return (stat as HTMLElement).querySelector('.stat-num')?.textContent ?? '';
		};
		expect(statValue('Completed')).toBe('3');
		expect(statValue('Failed')).toBe('1');
		expect(statValue('Canceled')).toBe('2');
	});

	// Packet 20: the Earnings panel must state the honest contract —
	// recorded, payouts coming later — and must NOT call the dead
	// driffs-era fetch_earnings IPC (its /api/operator-earnings route
	// does not exist in farm-web).
	// A-3 (audit U-10): the onboarding link used to be derived from the dispatch
	// URL and pointed at a driffs dev server (localhost:5173). Pairing lives at
	// the farm's devices page — the same URL the CLI prints (main.rs).
	it('"Connect this Mac" links to the production devices page and opens it in the system browser', async () => {
		render(<App />);
		const link = await screen.findByRole('link', {name: /connect this mac/i});
		expect(link).toHaveAttribute('href', 'https://decent-render.farm/devices');
		fireEvent.click(link);
		await waitFor(() => {
			expect(shellOpenMock).toHaveBeenCalledWith('https://decent-render.farm/devices');
		});
		expect(shellOpenMock).toHaveBeenCalledTimes(1);
		// The old IPC built `<appUrl>/settings/devices` (a 404 on the farm) —
		// it must not be called at all.
		const calls = invokeMock.mock.calls.map(([cmd]) => cmd as string);
		expect(calls).not.toContain('open_pairing_page');
	});

	it('earnings panel states the not-wired-up contract and makes no earnings IPC call', async () => {
		render(<App />);

		await waitFor(() => {
			expect(screen.getByText(/payouts are not yet available/i)).toBeInTheDocument();
		});
		const calls = invokeMock.mock.calls.map(([cmd]) => cmd as string);
		expect(calls).not.toContain('fetch_earnings');
		expect(screen.queryByText(/spendable/i)).not.toBeInTheDocument();
	});
});
