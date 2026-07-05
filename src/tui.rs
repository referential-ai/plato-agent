use crate::daemon::protocol::{HelloResult, SessionSummary, TranscriptReadResult};
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiState {
    pub workspace_root: String,
    pub socket_path: String,
    pub connection: ConnectionState,
    pub sessions: Vec<SessionSummary>,
    pub transcript: TranscriptState,
    pub active_run: Option<ActiveRunView>,
    pub live_events: Vec<LiveEventLine>,
    pub composer: String,
    pub status_message: Option<String>,
    pub stream_warning: Option<String>,
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
            active_run: None,
            live_events: Vec::new(),
            composer: String::new(),
            status_message: None,
            stream_warning: None,
        }
    }

    pub fn disconnected(workspace_root: String, socket_path: String, error: String) -> Self {
        Self {
            workspace_root,
            socket_path,
            connection: ConnectionState::Disconnected { error },
            sessions: Vec::new(),
            transcript: TranscriptState::None,
            active_run: None,
            live_events: Vec::new(),
            composer: String::new(),
            status_message: None,
            stream_warning: None,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRunView {
    pub run_id: String,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveEventLine {
    pub offset: Option<u64>,
    pub text: String,
}

impl LiveEventLine {
    pub fn new(offset: Option<u64>, text: impl Into<String>) -> Self {
        Self {
            offset,
            text: text.into(),
        }
    }
}

pub fn live_event_line(value: &Value) -> LiveEventLine {
    let offset = value.get("offset").and_then(Value::as_u64);
    let event = value.get("event").unwrap_or(value);
    let text = match event.get("kind").and_then(Value::as_str) {
        Some("ledger") => ledger_event_line(event),
        Some("approval_requested") => {
            let tool_name = event
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown tool");
            let effect = event
                .get("effect")
                .and_then(Value::as_str)
                .unwrap_or("unknown effect");
            format!("approval pending {tool_name} ({effect})")
        }
        Some(kind) => kind.into(),
        None => serde_json::to_string(event).unwrap_or_else(|_| "unrenderable event".into()),
    };
    LiveEventLine::new(offset, text)
}

fn ledger_event_line(event: &Value) -> String {
    let event_name = event
        .pointer("/record/event/event")
        .and_then(Value::as_str)
        .unwrap_or("ledger event");
    match event_name {
        "model_responded" => "assistant response".into(),
        "tool_call_proposed" => {
            let tool = event
                .pointer("/record/event/call/tool")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            format!("tool proposed {tool}")
        }
        "tool_finished" => "tool finished".into(),
        "run_finished" => "run finished".into(),
        "run_failed" => "run failed".into(),
        other => other.replace('_', " "),
    }
}

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let [header, body, events, footer] = vertical(frame.area());
    render_header(frame, header, state);
    render_body(frame, body, state);
    render_live_events(frame, events, state);
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

fn render_live_events(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let mut lines = Vec::new();
    if let Some(active) = &state.active_run {
        lines.push(Line::from(vec![
            Span::styled(
                active.status.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {}", active.run_id)),
        ]));
    }
    if let Some(message) = &state.status_message {
        lines.push(Line::from(message.clone()));
    }
    if let Some(warning) = &state.stream_warning {
        lines.push(Line::from(vec![
            Span::styled(
                "stream warning",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {warning}")),
        ]));
    }
    if state.live_events.is_empty() {
        lines.push(Line::from("No live events."));
    } else {
        lines.extend(state.live_events.iter().map(|event| match event.offset {
            Some(offset) => Line::from(format!("#{offset} {}", event.text)),
            None => Line::from(event.text.clone()),
        }));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Live Events"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let composer = if state.composer.is_empty() {
        ">".into()
    } else {
        format!("> {}", state.composer)
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(composer),
            Line::from("Enter submits. r reconnects. q exits. Slice 2: approvals/cancel pending."),
        ])
        .block(Block::default().borders(Borders::ALL).title("Composer")),
        area,
    );
}

fn vertical(area: Rect) -> [Rect; 4] {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(6),
            Constraint::Length(7),
            Constraint::Length(4),
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
        assert!(output.contains("approvals/cancel pending"));
    }

    #[test]
    fn renders_active_run_composer_and_live_events() {
        let mut state = TuiState::connected(
            "/tmp/work".into(),
            "/tmp/agent.sock".into(),
            HelloResult {
                daemon_version: "0.1.0".into(),
                workspace_id: "work-1234".into(),
                ledger_path: "/tmp/agent.db".into(),
                capabilities: vec![],
            },
            Vec::new(),
            TranscriptState::None,
        );
        state.active_run = Some(ActiveRunView {
            run_id: "run_1".into(),
            status: "running".into(),
        });
        state.composer = "summarize this file".into();
        state
            .live_events
            .push(LiveEventLine::new(Some(2), "assistant response"));

        let output = render_to_text(&state);

        assert!(output.contains("running"));
        assert!(output.contains("run_1"));
        assert!(output.contains("#2 assistant response"));
        assert!(output.contains("> summarize this file"));
    }

    #[test]
    fn renders_stream_warning() {
        let mut state = TuiState::connected(
            "/tmp/work".into(),
            "/tmp/agent.sock".into(),
            HelloResult {
                daemon_version: "0.1.0".into(),
                workspace_id: "work-1234".into(),
                ledger_path: "/tmp/agent.db".into(),
                capabilities: vec![],
            },
            Vec::new(),
            TranscriptState::None,
        );
        state.stream_warning = Some("lagged; transcript recovered".into());

        let output = render_to_text(&state);

        assert!(output.contains("stream warning"));
        assert!(output.contains("lagged"));
    }

    #[test]
    fn formats_daemon_event_lines() {
        let approval = live_event_line(&serde_json::json!({
            "offset": 4,
            "event": {
                "kind": "approval_requested",
                "tool_name": "file.write",
                "effect": "WorkspaceWrite"
            }
        }));
        let ledger = live_event_line(&serde_json::json!({
            "offset": 5,
            "event": {
                "kind": "ledger",
                "record": {
                    "event": {
                        "event": "tool_call_proposed",
                        "call": {
                            "tool": "file.read"
                        }
                    }
                }
            }
        }));

        assert_eq!(
            approval,
            LiveEventLine::new(Some(4), "approval pending file.write (WorkspaceWrite)")
        );
        assert_eq!(
            ledger,
            LiveEventLine::new(Some(5), "tool proposed file.read")
        );
    }

    fn render_to_text(state: &TuiState) -> String {
        render_snapshot(state, 100, 24).unwrap()
    }
}
