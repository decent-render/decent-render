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

		expect(screen.getByText('3')).toBeInTheDocument();
		expect(screen.getByText('1')).toBeInTheDocument();
		expect(screen.getByText('2')).toBeInTheDocument();
	});
});
