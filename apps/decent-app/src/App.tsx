import {useEffect, useState, useRef, useCallback} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {listen, type UnlistenFn} from '@tauri-apps/api/event';

// ── Types (mirror supervisor-core/src/status.rs) ───────────────────────────

type ConnectionState =
	| 'disconnected'
	| 'connecting'
	| 'connected'
	| 'registered';

type JobPhase =
	| 'downloading'
	| 'rendering'
	| 'uploading'
	| 'done'
	| 'failed'
	| 'canceled';

interface NodeIdentity {
	chip: string;
	platform: string;
	protocolVersion: number;
	supervisorVersion: string;
}

interface JobStatus {
	id: string;
	tier: string;
	progress: number;
	phase: JobPhase;
}

interface SupervisorStatus {
	connection: ConnectionState;
	dispatchUrl: string | null;
	nodeIdentity: NodeIdentity | null;
	currentJob: JobStatus | null;
	jobsCompleted: number;
	jobsFailed: number;
	jobsCanceled: number;
	lastError: string | null;
	allowRealJobs: boolean;
}

interface LogLine {
	timestampMs: number;
	level: 'debug' | 'info' | 'warn' | 'error';
	message: string;
}

interface AppConfig {
	dispatchUrl: string;
	workdirRoot: string | null;
	allowRealJobsDefault: boolean;
}

// ── Connection state colors ────────────────────────────────────────────────

const stateColor = (s: ConnectionState): string => {
	switch (s) {
		case 'registered':
			return '#4ade80';
		case 'connected':
			return '#60a5fa';
		case 'connecting':
			return '#fbbf24';
		case 'disconnected':
			return '#f87171';
	}
};

const phaseLabel = (p: JobPhase): string =>
	({downloading: 'Downloading', rendering: 'Rendering', uploading: 'Uploading', done: 'Done', failed: 'Failed', canceled: 'Canceled'}[p]);

// ── Component ───────────────────────────────────────────────────────────────

export default function App() {
	const [status, setStatus] = useState<SupervisorStatus | null>(null);
	const [logs, setLogs] = useState<LogLine[]>([]);
	const [config, setConfig] = useState<AppConfig | null>(null);
	const [dispatchUrl, setDispatchUrl] = useState('ws://localhost:8790/ws');
	const [token, setToken] = useState('');
	const [allowRealJobs, setAllowRealJobs] = useState(false);
	const [connecting, setConnecting] = useState(false);
	const logEndRef = useRef<HTMLDivElement>(null);

	// Load config + token on mount.
	useEffect(() => {
		(async () => {
			const cfg = await invoke<AppConfig>('get_config');
			setConfig(cfg);
			setDispatchUrl(cfg.dispatchUrl);
			setAllowRealJobs(await invoke<boolean>('get_allow_real_jobs'));
			setStatus(await invoke<SupervisorStatus>('get_status'));
			setToken(await invoke<string>('get_token'));
		})();
	}, []);

	// Listen for status + log events.
	useEffect(() => {
		let unlistenStatus: UnlistenFn | undefined;
		let unlistenLog: UnlistenFn | undefined;

		(async () => {
			unlistenStatus = await listen<SupervisorStatus>('status-update', (e) => {
				setStatus(e.payload);
				setAllowRealJobs(e.payload.allowRealJobs);
			});
			unlistenLog = await listen<LogLine>('log-line', (e) => {
				setLogs((prev) => [...prev.slice(-200), e.payload]);
			});
		})();

		return () => {
			unlistenStatus?.();
			unlistenLog?.();
		};
	}, []);

	// Auto-scroll log.
	useEffect(() => {
		logEndRef.current?.scrollIntoView({behavior: 'smooth'});
	}, [logs]);

	const handleStart = useCallback(async () => {
		if (!token.trim()) {
			alert('Please enter a worker token');
			return;
		}
		setConnecting(true);
		invoke('save_app_config', {
			dispatchUrl,
			workdirRoot: null,
			allowRealJobsDefault: allowRealJobs,
		});
		invoke('save_token_cmd', {token});
		try {
			await invoke('start_connection', {dispatchUrl, token});
		} catch (e) {
			alert(`Failed to start: ${e}`);
		} finally {
			setConnecting(false);
		}
	}, [dispatchUrl, token, allowRealJobs]);

	const handleStop = useCallback(async () => {
		await invoke('stop_connection');
	}, []);

	const handleToggleAllow = useCallback(async (value: boolean) => {
		setAllowRealJobs(value);
		await invoke('set_allow_real_jobs', {value});
	}, []);

	const handlePairDevice = useCallback(async () => {
		// Extract the origin from the dispatch URL to construct the app URL.
		// ws://localhost:8790/ws → http://localhost:5173 (driffs dev server)
		// In production this would be the driffs domain.
		const appUrl = dispatchUrl.startsWith('ws://localhost')
			? 'http://localhost:5173'
			: dispatchUrl.replace(/^ws/, 'http').replace(/:\d+\/ws$/, ':5173');
		await invoke('open_pairing_page', {appUrl});
	}, [dispatchUrl]);

	const isConnected = status?.connection === 'connected' || status?.connection === 'registered';
	const currentJob = status?.currentJob;

	return (
		<div className="app">
			<header className="header">
				<h1>Decent Render</h1>
				<div className="connection-badge">
					<span className="dot" style={{background: stateColor(status?.connection ?? 'disconnected')}} />
					<span>{(status?.connection ?? 'disconnected').toUpperCase()}</span>
				</div>
			</header>

			{/* Connection card */}
			<section className="card">
				<h2>Connection</h2>
				{status?.nodeIdentity && (
					<div className="identity">
						<span>{status.nodeIdentity.chip}</span>
						<span className="badge">{status.nodeIdentity.platform}</span>
						<span className="badge">v{status.nodeIdentity.protocolVersion}</span>
						<span className="badge">{status.nodeIdentity.supervisorVersion}</span>
					</div>
				)}
				{status?.lastError && <div className="error">{status.lastError}</div>}
				<div className="form-row">
					<label>Dispatch URL</label>
					<input
						type="text"
						value={dispatchUrl}
						onChange={(e) => setDispatchUrl(e.target.value)}
						disabled={isConnected}
						placeholder="ws://localhost:8790/ws"
					/>
				</div>
				<div className="form-row">
					<label>Worker Token</label>
					<input
						type="password"
						value={token}
						onChange={(e) => setToken(e.target.value)}
						disabled={isConnected}
						placeholder="JWT worker token"
					/>
				</div>
				<div className="actions">
					<button className="btn-primary" onClick={handleStart} disabled={isConnected || connecting}>
						{connecting ? 'Starting…' : 'Start'}
					</button>
					<button className="btn-danger" onClick={handleStop} disabled={!isConnected}>
						Stop
					</button>
				</div>
				{!isConnected && (
					<div className="pair-device">
						<button className="btn-link" onClick={handlePairDevice}>
							Connect this Mac →
						</button>
						<span className="hint">
							Opens your browser to create a device token. Copy it back here.
						</span>
					</div>
				)}
			</section>

			{/* Controls */}
			<section className="card">
				<h2>Controls</h2>
				<label className="toggle">
					<input
						type="checkbox"
						checked={allowRealJobs}
						onChange={(e) => handleToggleAllow(e.target.checked)}
					/>
					<span>Accept real render jobs</span>
				</label>
				<p className="hint">
					When OFF, the node registers and heartbeats but refuses all jobAssignments.
					Toggle ON to accept renders. This is the same safety gate as <code>--allow-real-jobs</code> on the CLI.
				</p>
			</section>

			{/* Current job */}
			<section className="card">
				<h2>Current Job</h2>
				{currentJob ? (
					<div className="job">
						<div className="job-header">
							<span className="job-id">{currentJob.id}</span>
							<span className="badge tier">{currentJob.tier}</span>
							<span className="badge phase">{phaseLabel(currentJob.phase)}</span>
						</div>
						<div className="progress-bar-container">
							<div
								className="progress-bar"
								style={{width: `${Math.round(currentJob.progress * 100)}%`}}
							/>
							<span className="progress-label">
								{Math.round(currentJob.progress * 100)}%
							</span>
						</div>
					</div>
				) : (
					<div className="empty-state">No active job — idle</div>
				)}
			</section>

			{/* Session stats */}
			<section className="card">
				<h2>Session Stats</h2>
				<div className="stats">
					<div className="stat">
						<span className="stat-num green">{status?.jobsCompleted ?? 0}</span>
						<span className="stat-label">Completed</span>
					</div>
					<div className="stat">
						<span className="stat-num red">{status?.jobsFailed ?? 0}</span>
						<span className="stat-label">Failed</span>
					</div>
					<div className="stat">
						<span className="stat-num yellow">{status?.jobsCanceled ?? 0}</span>
						<span className="stat-label">Canceled</span>
					</div>
				</div>
			</section>

			{/* Log tail */}
			<section className="card log-section">
				<h2>Log Tail</h2>
				<div className="log-tail">
					{logs.length === 0 && <div className="empty-state">No log output yet</div>}
					{logs.map((line, i) => (
						<div key={i} className={`log-line ${line.level}`}>
							<span className="log-time">
								{new Date(line.timestampMs).toLocaleTimeString()}
							</span>
							<span className={`log-level ${line.level}`}>{line.level.toUpperCase()}</span>
							<span className="log-msg">{line.message}</span>
						</div>
					))}
					<div ref={logEndRef} />
				</div>
			</section>

			<footer className="footer">
				<span>Purge rule enforced by core — the app cannot bypass workdir deletion.</span>
			</footer>
		</div>
	);
}
