use crate::daemon::protocol::{HelloResult, SessionSummary, TranscriptReadResult};
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiState {
    pub workspace_root: String,
    pub socket_path: String,
    pub connection: ConnectionState,
    pub sessions: Vec<SessionSummary>,
    pub transcript: TranscriptState,
}

impl TuiState {
    pub fn connected(
        workspace_root: String,
        socket_path: String,
        hello: HelloResult,
        sessions: Vec<SessionSummary>,
        transcript: TranscriptState,
    ) -> Self {
        Self {
            workspace_root,
            socket_path,
            connection: ConnectionState::Connected {
                workspace_id: hello.workspace_id,
                daemon_version: hello.daemon_version,
                ledger_path: hello.ledger_path,
            },
            sessions,
            transcript,
        }
    }

    pub fn disconnected(workspace_root: String, socket_path: String, error: String) -> Self {
        Self {
            workspace_root,
            socket_path,
            connection: ConnectionState::Disconnected { error },
            sessions: Vec::new(),
            transcript: TranscriptState::None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Connected {
        workspace_id: String,
        daemon_version: String,
        ledger_path: String,
    },
    Disconnected {
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptView {
    pub run_id: String,
    pub content: String,
}

impl From<TranscriptReadResult> for TranscriptView {
    fn from(transcript: TranscriptReadResult) -> Self {
        Self {
            run_id: transcript.run_id,
            content: transcript.transcript,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptState {
    None,
    Loaded(TranscriptView),
    Unavailable { run_id: String, error: String },
}

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let [header, body, footer] = vertical(frame.area());
    render_header(frame, header, state);
    render_body(frame, body, state);
    render_footer(frame, footer, state);
}

pub fn render_snapshot(state: &TuiState, width: u16, height: u16) -> std::io::Result<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render(frame, state))?;
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    let mut output = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    Ok(output)
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let status = match &state.connection {
        ConnectionState::Connected {
            workspace_id,
            daemon_version,
            ..
        } => vec![
            Span::styled("connected", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!(" workspace={workspace_id} daemon={daemon_version}")),
        ],
        ConnectionState::Disconnected { error } => vec![
            Span::styled(
                "daemon unavailable",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {error}")),
        ],
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Plato TUI"),
            Line::from(status),
            Line::from(format!("socket: {}", state.socket_path)),
        ])
        .block(Block::default().borders(Borders::ALL).title("Status")),
        area,
    );
}

fn render_body(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let [sessions_area, transcript_area] = horizontal(area);
    render_sessions(frame, sessions_area, state);
    render_transcript(frame, transcript_area, state);
}

fn render_sessions(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let lines = if state.sessions.is_empty() {
        vec![Line::from("No daemon-lifetime sessions.")]
    } else {
        state
            .sessions
            .iter()
            .map(|session| {
                Line::from(vec![
                    Span::styled(
                        session.status.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {}", session.run_id)),
                ])
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Sessions"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let text = match &state.transcript {
        TranscriptState::Loaded(transcript) => {
            format!("run: {}\n\n{}", transcript.run_id, transcript.content)
        }
        TranscriptState::Unavailable { run_id, error } => {
            format!("Transcript unavailable for run {run_id}.\n\n{error}")
        }
        TranscriptState::None if matches!(state.connection, ConnectionState::Connected { .. }) => {
            "No transcript selected. Start with --run <RUN_ID>.".into()
        }
        TranscriptState::None => format!(
            "Start plato-agentd manually, then reconnect.\n\ncargo run --bin plato-agentd -- --workspace {}",
            state.workspace_root
        ),
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("Transcript"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, _state: &TuiState) {
    frame.render_widget(
        Paragraph::new("q exits. r reconnects. Slice 1 is read-only: no run start, append, approval, or cancel.")
            .block(Block::default().borders(Borders::ALL).title("Keys")),
        area,
    );
}

fn vertical(area: Rect) -> [Rect; 3] {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .areas(area)
}

fn horizontal(area: Rect) -> [Rect; 2] {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(20)])
        .areas(area)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_connected_sessions_and_transcript() {
        let state = TuiState::connected(
            "/tmp/work".into(),
            "/tmp/agent.sock".into(),
            HelloResult {
                daemon_version: "0.1.0".into(),
                workspace_id: "work-1234".into(),
                ledger_path: "/tmp/agent.db".into(),
                capabilities: vec![],
            },
            vec![SessionSummary {
                session_id: "run_1".into(),
                run_id: "run_1".into(),
                status: "finished".into(),
                ledger_path: "/tmp/agent.db".into(),
            }],
            TranscriptState::Loaded(
                TranscriptReadResult {
                    run_id: "run_1".into(),
                    transcript: "final_phase: Finished".into(),
                }
                .into(),
            ),
        );

        let output = render_to_text(&state);

        assert!(output.contains("connected"));
        assert!(output.contains("run_1"));
        assert!(output.contains("final_phase"));
    }

    #[test]
    fn renders_transcript_error_for_selected_run() {
        let state = TuiState::connected(
            "/tmp/work".into(),
            "/tmp/agent.sock".into(),
            HelloResult {
                daemon_version: "0.1.0".into(),
                workspace_id: "work-1234".into(),
                ledger_path: "/tmp/agent.db".into(),
                capabilities: vec![],
            },
            vec![SessionSummary {
                session_id: "run_1".into(),
                run_id: "run_1".into(),
                status: "failed".into(),
                ledger_path: "/tmp/agent.db".into(),
            }],
            TranscriptState::Unavailable {
                run_id: "run_1".into(),
                error: "run not found: run_1".into(),
            },
        );

        let output = render_to_text(&state);

        assert!(output.contains("Transcript unavailable"));
        assert!(output.contains("run_1"));
    }

    #[test]
    fn renders_daemon_unavailable_guidance() {
        let state = TuiState::disconnected(
            "/tmp/work".into(),
            "/tmp/agent.sock".into(),
            "connection refused".into(),
        );

        let output = render_to_text(&state);

        assert!(output.contains("daemon unavailable"));
        assert!(output.contains("cargo run --bin plato-agentd"));
        assert!(output.contains("Slice 1 is read-only"));
    }

    fn render_to_text(state: &TuiState) -> String {
        render_snapshot(state, 100, 24).unwrap()
    }
}
