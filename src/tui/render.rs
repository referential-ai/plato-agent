use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use super::{ApprovalModalView, ConnectionState, TranscriptState, TuiState};

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let [header, body, events, footer] = vertical(frame.area());
    render_header(frame, header, state);
    render_body(frame, body, state);
    render_live_events(frame, events, state);
    render_footer(frame, footer, state);
    if let Some(approval) = &state.approval {
        render_approval_modal(frame, frame.area(), approval);
    }
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
            Line::from(
                "Type text. Enter submits. r reconnects if disconnected. q exits empty composer. Ctrl-C cancels.",
            ),
        ])
        .block(Block::default().borders(Borders::ALL).title("Composer")),
        area,
    );
}

fn render_approval_modal(frame: &mut Frame<'_>, area: Rect, approval: &ApprovalModalView) {
    let area = centered_rect(74, 64, area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("run ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(approval.run_id.clone()),
        ]),
        Line::from(vec![
            Span::styled("call ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(approval.tool_call_id.clone()),
        ]),
        Line::from(vec![
            Span::styled("tool ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("{} ({})", approval.tool_name, approval.effect)),
        ]),
        Line::from(vec![
            Span::styled("reason ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(approval.reason.clone()),
        ]),
        Line::from(""),
        Line::from("g grant    d deny    Ctrl-C cancel run    q quit TUI"),
        Line::from(""),
    ];
    if let Some(diff_preview) = &approval.diff_preview {
        lines.push(Line::from("diff preview:"));
        lines.extend(diff_preview.lines().map(|line| Line::from(line.to_owned())));
    } else {
        lines.push(Line::from("input preview:"));
        lines.push(Line::from(approval.input_preview.clone()));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Approval"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
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
    use crate::daemon::protocol::{HelloResult, SessionSummary, TranscriptReadResult};

    use super::super::{ActiveRunView, LiveEventLine};

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
        assert!(output.contains("r reconnects if disconnected"));
        assert!(output.contains("q exits empty composer"));
        assert!(output.contains("Ctrl-C cancels"));
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
    fn renders_approval_modal() {
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
        state.approval = Some(ApprovalModalView {
            run_id: "run_1".into(),
            tool_call_id: "call_1".into(),
            tool_name: "file.write".into(),
            effect: "WorkspaceWrite".into(),
            reason: "file.write requires approval".into(),
            input_preview: r#"{"path":"scratch.txt"}"#.into(),
            diff_preview: None,
        });

        let output = render_to_text(&state);

        assert!(output.contains("Approval"));
        assert!(output.contains("file.write"));
        assert!(output.contains("WorkspaceWrite"));
        assert!(output.contains("scratch.txt"));
        assert!(output.contains("g grant"));
        assert!(output.contains("d deny"));
    }

    #[test]
    fn renders_approval_modal_diff_preview_when_present() {
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
        state.approval = Some(ApprovalModalView {
            run_id: "run_1".into(),
            tool_call_id: "call_1".into(),
            tool_name: "file.edit".into(),
            effect: "WorkspaceWrite".into(),
            reason: "file.edit requires approval".into(),
            input_preview: r#"{"path":"scratch.txt"}"#.into(),
            diff_preview: Some("--- a/scratch.txt\n+++ b/scratch.txt\n-old\n+new\n".into()),
        });

        let output = render_to_text(&state);

        assert!(output.contains("diff preview"));
        assert!(output.contains("--- a/scratch.txt"));
        assert!(output.contains("-old"));
        assert!(output.contains("+new"));
        assert!(!output.contains("input preview:"));
    }

    #[test]
    fn renders_approval_modal_controls_with_long_diff_preview() {
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
        let body = (0..40)
            .map(|line| format!("-old-{line}\n+new-{line}\n"))
            .collect::<String>();
        state.approval = Some(ApprovalModalView {
            run_id: "run_1".into(),
            tool_call_id: "call_1".into(),
            tool_name: "file.edit".into(),
            effect: "WorkspaceWrite".into(),
            reason: "file.edit requires approval".into(),
            input_preview: r#"{"path":"scratch.txt"}"#.into(),
            diff_preview: Some(format!("--- a/scratch.txt\n+++ b/scratch.txt\n{body}")),
        });

        let output = render_to_text(&state);

        assert!(output.contains("g grant"));
        assert!(output.contains("d deny"));
        assert!(output.contains("diff preview"));
        assert!(output.contains("--- a/scratch.txt"));
    }

    fn render_to_text(state: &TuiState) -> String {
        render_snapshot(state, 100, 24).unwrap()
    }
}
