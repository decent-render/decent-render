//! decent-node TUI — the primary node-local operator surface (W3.11).
//!
//! A live terminal dashboard over the supervisor's observability channels:
//! connection state, node identity, the current job + progress, job counters,
//! an update-available indicator, and a scrolling log tail. `q`/Esc quits.
//!
//! Architecture: `run()` is the blocking entry on the main thread. The async
//! connection loop runs in a sibling tokio task (spawned by the caller) and
//! pushes `SupervisorStatus` (watch) + `LogLine` (broadcast) into the channels
//! we read here. Quitting drops/sends the oneshot shutdown so the connection
//! task exits cleanly.
//!
//! The TUI is a foreground supervisor (like `start`), not a view onto the
//! daemon: it makes its own connection. Don't run it alongside an installed
//! daemon on the same machine (two sockets, one device token) — pause/uninstall
//! the daemon first, or run the TUI on a machine that isn't daemon-managed.

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
	disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::{broadcast, oneshot, watch};

use supervisor_core::status::{
	ConnectionState, JobPhase, LogLevel, LogLine, SupervisorStatus,
};

/// Max log lines kept in the on-screen ring buffer.
const LOG_RING: usize = 500;
/// Event-poll interval — also caps the render rate.
const TICK: Duration = Duration::from_millis(120);

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Run the TUI until the user quits (q/Esc). Restores the terminal on return
/// (including on error).
pub fn run(
	mut status_rx: watch::Receiver<SupervisorStatus>,
	mut log_rx: broadcast::Receiver<LogLine>,
	shutdown_tx: oneshot::Sender<()>,
) -> anyhow::Result<()> {
	let mut terminal = setup_terminal()?;
	let mut logs: VecDeque<LogLine> = VecDeque::with_capacity(LOG_RING);
	let mut list_state = ListState::default();
	list_state.select(None);

	// Keep the shutdown sender; send on clean quit. If we return early via `?`,
	// dropping it signals Closed to the connection loop → it exits too.
	let mut shutdown_tx = Some(shutdown_tx);

	let result = run_loop(
		&mut terminal,
		&mut status_rx,
		&mut log_rx,
		&mut logs,
		&mut list_state,
		&mut shutdown_tx,
	);

	// Always restore the terminal, even if the loop errored.
	let restore = restore_terminal(&mut terminal);
	result?;
	restore?;
	Ok(())
}

fn setup_terminal() -> io::Result<Term> {
	enable_raw_mode()?;
	io::stdout().execute(EnterAlternateScreen)?;
	let backend = CrosstermBackend::new(io::stdout());
	Terminal::new(backend)
}

fn restore_terminal(terminal: &mut Term) -> io::Result<()> {
	disable_raw_mode()?;
	execute!(io::stdout(), LeaveAlternateScreen)?;
	terminal.show_cursor()?;
	Ok(())
}

fn run_loop(
	terminal: &mut Term,
	status_rx: &mut watch::Receiver<SupervisorStatus>,
	log_rx: &mut broadcast::Receiver<LogLine>,
	logs: &mut VecDeque<LogLine>,
	list_state: &mut ListState,
	shutdown_tx: &mut Option<oneshot::Sender<()>>,
) -> anyhow::Result<()> {
	list_state.select_last();
	loop {
		// Drain any new log lines into the ring buffer (newest at the back).
		loop {
			match log_rx.try_recv() {
				Ok(line) => {
					if logs.len() >= LOG_RING {
						logs.pop_front();
					}
					logs.push_back(line);
				}
				Err(_) => break, // Empty | Closed | Lagged — resume next tick
			}
		}

		let status = status_rx.borrow().clone();
		terminal.draw(|f| draw(f, &status, logs, list_state))?;

		// Block up to TICK for input; if none, loop redraws (progress/logs).
		if !event::poll(TICK)? {
			continue;
		}
		if let Event::Key(key) = event::read()? {
			if key.kind != KeyEventKind::Press {
				continue;
			}
			match key.code {
				KeyCode::Char('q') | KeyCode::Esc => {
					if let Some(tx) = shutdown_tx.take() {
						let _ = tx.send(());
					}
					return Ok(());
				}
				_ => {}
			}
		}
	}
}

fn draw(
	frame: &mut Frame,
	status: &SupervisorStatus,
	logs: &VecDeque<LogLine>,
	list_state: &mut ListState,
) {
	let area = frame.area();
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(3), // title bar
			Constraint::Length(10), // status + job row
			Constraint::Min(8), // logs
			Constraint::Length(1), // footer
		])
		.split(area);

	draw_title_bar(frame, chunks[0], status);
	draw_status_and_job(frame, chunks[1], status);
	draw_logs(frame, chunks[2], logs, list_state);
	draw_footer(frame, chunks[3], status);
}

fn draw_title_bar(frame: &mut Frame, area: Rect, status: &SupervisorStatus) {
	let conn_color = match status.connection {
		ConnectionState::Registered => Color::Green,
		ConnectionState::Connected => Color::Cyan,
		ConnectionState::Connecting => Color::Yellow,
		ConnectionState::Disconnected => Color::Red,
	};
	let conn_label = format!("{:?}", status.connection); // Disconnected | ... | Registered
	let version = status
		.node_identity
		.as_ref()
		.map(|i| i.supervisor_version.as_str())
		.unwrap_or("decent-node");
	let update = match &status.update_available {
		Some(v) => format!("  ⚠ update available: {v}"),
		None => String::new(),
	};
	let line = Line::from(vec![
		Span::styled(
			format!(" {version} "),
			Style::default().add_modifier(Modifier::BOLD),
		),
		Span::raw("·"),
		Span::styled(format!(" {conn_label} "), Style::default().fg(conn_color)),
		Span::raw("·"),
		Span::styled(
			format!(
				" {} ",
				status.dispatch_url.as_deref().unwrap_or("no dispatch url")
			),
			Style::default().fg(Color::DarkGray),
		),
		Span::styled(update, Style::default().fg(Color::Yellow)),
	]);
	let block = Block::default().borders(Borders::ALL).style(
		Style::default()
			.bg(Color::Black)
			.add_modifier(Modifier::BOLD),
	);
	frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_status_and_job(frame: &mut Frame, area: Rect, status: &SupervisorStatus) {
	let row = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
		.split(area);
	draw_status_panel(frame, row[0], status);
	draw_job_panel(frame, row[1], status);
}

fn draw_status_panel(frame: &mut Frame, area: Rect, status: &SupervisorStatus) {
	let id = status.node_identity.as_ref();
	let mut lines: Vec<Line> = vec![
		kv(
			"connection",
			&format!("{:?}", status.connection),
			match status.connection {
				ConnectionState::Registered => Color::Green,
				ConnectionState::Connected => Color::Cyan,
				ConnectionState::Connecting => Color::Yellow,
				ConnectionState::Disconnected => Color::Red,
			},
		),
		kv(
			"real jobs",
			if status.allow_real_jobs { "enabled" } else { "disabled (smoke)" },
			if status.allow_real_jobs { Color::Green } else { Color::DarkGray },
		),
		kv("chip", id.map(|i| i.chip.as_str()).unwrap_or("—"), Color::Reset),
		kv(
			"platform",
			id.map(|i| i.platform.as_str()).unwrap_or("—"),
			Color::Reset,
		),
		kv(
			"protocol",
			id.map(|i| format!("v{}", i.protocol_version)).unwrap_or("—".to_string()),
			Color::Reset,
		),
		kv(
			"completed / failed / canceled",
			format!(
				"{} / {} / {}",
				status.jobs_completed,
				status.jobs_failed,
				status.jobs_canceled
			),
			Color::Reset,
		),
	];
	if let Some(e) = &status.last_error {
		lines.push(Line::from(vec![
			Span::styled("last error ", Style::default().fg(Color::DarkGray)),
			Span::styled(e.as_str(), Style::default().fg(Color::Red)),
		]));
	}

	let block = Block::default().borders(Borders::ALL).title("Status");
	frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_job_panel(frame: &mut Frame, area: Rect, status: &SupervisorStatus) {
	let block = Block::default().borders(Borders::ALL).title("Current job");
	match &status.current_job {
		None => {
			frame.render_widget(
				Paragraph::new("idle — waiting for a job")
					.style(Style::default().fg(Color::DarkGray))
					.block(block),
				area,
			);
		}
		Some(job) => {
			let inner = block.inner(area);
			frame.render_widget(Block::default().borders(Borders::ALL), area);
			let pct = (job.progress.clamp(0.0, 1.0) * 100.0) as u16;
			let phase_label = phase_text(job.phase);
			let phase_color = phase_color(job.phase);
			let chunks = Layout::default()
				.direction(Direction::Vertical)
				.constraints([Constraint::Length(3), Constraint::Min(0)])
				.split(inner);
			let info = Paragraph::new(vec![
				Line::from(vec![
					Span::styled("job ", Style::default().fg(Color::DarkGray)),
					Span::raw(&job.id),
				]),
				Line::from(vec![
					Span::styled("tier ", Style::default().fg(Color::DarkGray)),
					Span::raw(&job.tier),
					Span::raw("   "),
					Span::styled("phase ", Style::default().fg(Color::DarkGray)),
					Span::styled(phase_label, Style::default().fg(phase_color)),
				]),
			]);
			frame.render_widget(info, chunks[0]);
			let gauge = Gauge::default()
				.block(Block::default().borders(Borders::ALL).title("progress"))
				.gauge_style(gauge_style(job.phase))
				.percent(pct);
			frame.render_widget(gauge, chunks[1]);
		}
	}
}

fn draw_logs(
	frame: &mut Frame,
	area: Rect,
	logs: &VecDeque<LogLine>,
	list_state: &mut ListState,
) {
	let block = Block::default().borders(Borders::ALL).title("Log");
	let items: Vec<ListItem> = logs
		.iter()
		.map(|l| {
			let (tag, color) = level_tag(l.level);
			ListItem::new(Line::from(vec![
				Span::styled(format!("{tag} "), Style::default().fg(color)),
				Span::raw(&l.message),
			]))
		})
		.collect();
	let list = List::new(items)
		.block(block)
		.highlight_style(Style::default().bg(Color::DarkGray));
	frame.render_stateful_widget(list, area, list_state);
}

fn draw_footer(frame: &mut Frame, area: Rect, status: &SupervisorStatus) {
	let real = if status.allow_real_jobs { "" } else { "  (smoke mode — not accepting jobs)" };
	let text = format!(" q/Esc quit  ·  pause rendering via Ctrl-C then `decent-node resume`{real}");
	frame.render_widget(
		Paragraph::new(text)
			.style(Style::default().fg(Color::DarkGray))
			.alignment(Alignment::Left),
		area,
	);
}

// ── helpers ────────────────────────────────────────────────────────────────

fn kv(label: &str, value: impl std::fmt::Display, value_color: Color) -> Line<'static> {
	Line::from(vec![
		Span::styled(format!("{label:<28}"), Style::default().fg(Color::DarkGray)),
		Span::styled(value.to_string(), Style::default().fg(value_color)),
	])
}

fn phase_text(phase: JobPhase) -> &'static str {
	match phase {
		JobPhase::Downloading => "downloading",
		JobPhase::Rendering => "rendering",
		JobPhase::Uploading => "uploading",
		JobPhase::Done => "done",
		JobPhase::Failed => "failed",
		JobPhase::Canceled => "canceled",
	}
}

fn phase_color(phase: JobPhase) -> Color {
	match phase {
		JobPhase::Rendering | JobPhase::Downloading | JobPhase::Uploading => Color::Cyan,
		JobPhase::Done => Color::Green,
		JobPhase::Failed | JobPhase::Canceled => Color::Red,
	}
}

fn gauge_style(phase: JobPhase) -> Style {
	match phase {
		JobPhase::Rendering | JobPhase::Downloading | JobPhase::Uploading => {
			Style::default().fg(Color::Cyan)
		}
		JobPhase::Done => Style::default().fg(Color::Green),
		JobPhase::Failed | JobPhase::Canceled => Style::default().fg(Color::Red),
	}
}

fn level_tag(level: LogLevel) -> (&'static str, Color) {
	match level {
		LogLevel::Error => ("ERROR", Color::Red),
		LogLevel::Warn => ("WARN ", Color::Yellow),
		LogLevel::Info => ("INFO ", Color::Cyan),
		LogLevel::Debug => ("DEBUG", Color::DarkGray),
	}
}
