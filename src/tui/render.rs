use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use super::{ApprovalModalView, ConnectionState, LiveEventKind, TranscriptState, TuiState};

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let [header, history, status, composer] = vertical(frame.area(), state);
    render_header(frame, header, state);
    render_history(frame, history, state);
    render_status_rule(frame, status, state);
    render_composer(frame, composer, state);
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

fn render_history(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let mut lines = history_lines(state);
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    let visible = visible_lines(lines, area.height, state.scroll_offset);
    frame.render_widget(Paragraph::new(visible).wrap(Wrap { trim: false }), area);
}

fn history_lines(state: &TuiState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match &state.transcript {
        TranscriptState::Loaded(transcript) => {
            lines.push(Line::from(vec![
                Span::styled("run ", Style::default().fg(Color::Yellow)),
                Span::raw(transcript.run_id.clone()),
            ]));
            lines.push(Line::from(""));
            lines.extend(
                transcript
                    .content
                    .lines()
                    .map(|line| Line::from(line.to_owned())),
            );
        }
        TranscriptState::Unavailable { run_id, error } => {
            lines.push(Line::from(vec![
                Span::styled(
                    "transcript unavailable ",
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(run_id.clone()),
            ]));
            lines.push(Line::from(error.clone()));
        }
        TranscriptState::None if matches!(state.connection, ConnectionState::Connected { .. }) => {
            lines.extend(intro_lines(state));
        }
        TranscriptState::None => {
            lines.push(Line::from(vec![Span::styled(
                "daemon unavailable",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )]));
            if let ConnectionState::Disconnected { error } = &state.connection {
                lines.push(Line::from(error.clone()));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Start plato-agentd manually, then press r to reconnect.",
            ));
            lines.push(Line::from(format!(
                "cargo run --bin plato-agentd -- --workspace {}",
                state.workspace_root
            )));
        }
    }

    append_live_transcript(&mut lines, state);
    append_queue_preview(&mut lines, state);
    lines
}

fn intro_lines(state: &TuiState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![Span::styled(
            "Plato Agent",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from("Local Rust agent runtime"),
        Line::from(""),
    ];

    if let ConnectionState::Connected {
        workspace_id,
        daemon_version,
        ledger_path,
    } = &state.connection
    {
        lines.extend([
            Line::from(vec![
                Span::styled("workspace ", Style::default().fg(Color::DarkGray)),
                Span::raw(workspace_id.clone()),
            ]),
            Line::from(vec![
                Span::styled("daemon    ", Style::default().fg(Color::DarkGray)),
                Span::raw(daemon_version.clone()),
            ]),
            Line::from(vec![
                Span::styled("ledger    ", Style::default().fg(Color::DarkGray)),
                Span::raw(ledger_path.clone()),
            ]),
            Line::from(vec![
                Span::styled("cwd       ", Style::default().fg(Color::DarkGray)),
                Span::raw(state.workspace_root.clone()),
            ]),
            Line::from(""),
            Line::from(format!(
                "{} session{}",
                state.sessions.len(),
                plural(state.sessions.len())
            )),
        ]);
    }

    lines
}

fn append_live_transcript(lines: &mut Vec<Line<'static>>, state: &TuiState) {
    let has_activity = state.active_run.is_some()
        || state.status_message.is_some()
        || state.stream_warning.is_some()
        || !state.live_events.is_empty();
    if !has_activity {
        return;
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "transcript",
        Style::default().fg(Color::Yellow),
    )]));

    if let Some(active) = &state.active_run {
        lines.push(status_row(format!("{} {}", active.status, active.run_id)));
    }
    if let Some(message) = &state.status_message {
        lines.push(status_row(message.clone()));
    }
    if let Some(warning) = &state.stream_warning {
        lines.push(warning_row(format!("stream warning {warning}")));
    }
    lines.extend(state.live_events.iter().map(event_row));
}

fn append_queue_preview(lines: &mut Vec<Line<'static>>, state: &TuiState) {
    if state.queued_messages.is_empty() {
        return;
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "queued",
        Style::default().fg(Color::Yellow),
    )]));
    lines.extend(
        state
            .queued_messages
            .iter()
            .enumerate()
            .map(|(index, message)| Line::from(format!("{} {}", index + 1, message))),
    );
}

fn render_status_rule(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    frame.render_widget(Paragraph::new(status_rule(state)), area);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    frame.render_widget(Paragraph::new(header_rule(state)), area);
}

fn header_rule(state: &TuiState) -> Line<'static> {
    let run_status = state
        .active_run
        .as_ref()
        .map(|run| run.status.as_str())
        .unwrap_or("ready");
    let elapsed = state
        .active_run_elapsed_secs
        .map(format_elapsed)
        .unwrap_or_else(|| "0s".into());
    let model = state.active_model.as_deref().unwrap_or("model pending");
    let workspace = match &state.connection {
        ConnectionState::Connected { workspace_id, .. } => workspace_id.as_str(),
        ConnectionState::Disconnected { .. } => "offline",
    };
    Line::from(vec![
        Span::styled(
            "Plato Agent",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " | {run_status} | {elapsed} | {model} | {workspace}"
        )),
    ])
}

fn status_rule(state: &TuiState) -> Line<'static> {
    let queued = state.queued_messages.len();
    let status = match &state.connection {
        ConnectionState::Connected { workspace_id, .. } => {
            let run_status = state
                .active_run
                .as_ref()
                .map(|run| run.status.as_str())
                .unwrap_or("ready");
            format!(
                "-- {run_status} | plato | queued {queued} | {} session{} | {} -- {}",
                state.sessions.len(),
                plural(state.sessions.len()),
                workspace_id,
                state.workspace_root
            )
        }
        ConnectionState::Disconnected { .. } => format!(
            "-- offline | plato | press r to reconnect -- {}",
            state.workspace_root
        ),
    };
    Line::from(Span::styled(status, Style::default().fg(Color::DarkGray)))
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let mut lines = if state.composer.is_empty() {
        vec![Line::from(vec![
            Span::styled(
                ">",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled("|", Style::default().fg(Color::Yellow)),
            Span::raw(" "),
            Span::styled(
                "Try \"read README.md and summarize it\"",
                Style::default().fg(Color::DarkGray),
            ),
        ])]
    } else {
        composer_with_cursor(state)
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let prefix = if index == 0 { ">" } else { "|" };
                Line::from(vec![
                    Span::styled(
                        prefix,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {line}")),
                ])
            })
            .collect()
    };
    lines.push(Line::from(Span::styled(
        format!(
            "Enter submits | Alt/Shift-Enter newline | PgUp/PgDown scroll | queued {}",
            state.queued_messages.len()
        ),
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn event_row(event: &super::LiveEventLine) -> Line<'static> {
    match event.kind {
        LiveEventKind::User => role_row("user", Color::Cyan, &event.text),
        LiveEventKind::Assistant | LiveEventKind::AssistantDelta => {
            role_row("assistant", Color::Green, &event.text)
        }
        LiveEventKind::Tool => role_row("tool", Color::Magenta, &event.text),
        LiveEventKind::Status => status_row(offset_text(event)),
        LiveEventKind::Warning => warning_row(offset_text(event)),
    }
}

fn role_row(role: &'static str, color: Color, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{role:<9} "), Style::default().fg(color)),
        Span::raw(text.to_owned()),
    ])
}

fn status_row(text: impl Into<String>) -> Line<'static> {
    role_row("status", Color::DarkGray, &text.into())
}

fn warning_row(text: impl Into<String>) -> Line<'static> {
    role_row("warning", Color::Red, &text.into())
}

fn offset_text(event: &super::LiveEventLine) -> String {
    match event.offset {
        Some(offset) => format!("#{offset} {}", event.text),
        None => event.text.clone(),
    }
}

fn visible_lines(
    lines: Vec<Line<'static>>,
    height: u16,
    scroll_offset: usize,
) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 || lines.len() <= height {
        return lines;
    }
    let end = lines.len().saturating_sub(scroll_offset).max(height);
    let start = end.saturating_sub(height);
    lines[start..end].to_vec()
}

fn composer_with_cursor(state: &TuiState) -> String {
    let mut draft = state.composer.clone();
    let mut cursor = state.composer_cursor.min(draft.len());
    while !draft.is_char_boundary(cursor) {
        cursor -= 1;
    }
    draft.insert(cursor, '|');
    draft
}

fn format_elapsed(seconds: u64) -> String {
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m{seconds:02}s")
    }
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
    } else if let Some(approval_preview) = &approval.approval_preview {
        lines.push(Line::from("approval preview:"));
        lines.extend(
            approval_preview
                .lines()
                .map(|line| Line::from(line.to_owned())),
        );
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

fn vertical(area: Rect, state: &TuiState) -> [Rect; 4] {
    let composer_height = composer_height(state);
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(composer_height),
        ])
        .areas(area)
}

fn composer_height(state: &TuiState) -> u16 {
    let draft_lines = if state.composer.is_empty() {
        1
    } else {
        state.composer.lines().count().max(1)
    };
    (draft_lines + 1).clamp(2, 6) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{HelloResult, SessionSummary, TranscriptReadResult};

    use super::super::{ActiveRunView, LiveEventLine};

    #[test]
    fn renders_intro_as_chat_surface() {
        let state = TuiState::connected(
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

        let output = render_to_text(&state);

        assert!(output.contains("Plato Agent"));
        assert!(output.contains("Local Rust agent runtime"));
        assert!(output.contains("work-1234"));
        assert!(output.contains("model pending"));
        assert!(output.contains("ready | plato"));
        assert!(output.contains("Try \"read README.md and summarize it\""));
        assert!(!output.contains("Status"));
        assert!(!output.contains("Sessions"));
        assert!(!output.contains("Live Events"));
        assert!(!output.contains("Composer"));
    }

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

        assert!(output.contains("ready"));
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

        assert!(output.contains("transcript unavailable"));
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
        assert!(output.contains("press r to reconnect"));
        assert!(output.contains("offline | plato"));
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
        state.composer_cursor = "summarize".len();
        state
            .live_events
            .push(LiveEventLine::assistant(Some(2), "assistant response"));

        let output = render_to_text(&state);

        assert!(output.contains("running"));
        assert!(output.contains("run_1"));
        assert!(output.contains("assistant response"));
        assert!(output.contains("> summarize| this file"));
    }

    #[test]
    fn renders_queue_preview_and_multiline_composer() {
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
        state.queued_messages = vec!["queued next".into()];
        state.composer = "first line\nsecond line".into();
        state.composer_cursor = state.composer.len();

        let output = render_to_text(&state);

        assert!(output.contains("queued"));
        assert!(output.contains("queued 1"));
        assert!(output.contains("1 queued next"));
        assert!(output.contains("> first line"));
        assert!(output.contains("| second line|"));
    }

    #[test]
    fn renders_typed_tool_and_status_rows() {
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
        state.active_model = Some("openrouter/auto".into());
        state.active_run_elapsed_secs = Some(65);
        state.live_events = vec![
            LiveEventLine::user("read README"),
            LiveEventLine::tool(Some(3), "file.read finished"),
            LiveEventLine::warning(Some(4), "approval pending shell.exec"),
        ];

        let output = render_to_text(&state);

        assert!(output.contains("1m05s"));
        assert!(output.contains("openrouter/auto"));
        assert!(output.contains("user"));
        assert!(output.contains("read README"));
        assert!(output.contains("tool"));
        assert!(output.contains("file.read finished"));
        assert!(output.contains("warning"));
        assert!(output.contains("approval pending shell.exec"));
    }

    #[test]
    fn renders_scrolled_transcript_window() {
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
        state.live_events = (0..30)
            .map(|index| LiveEventLine::status(Some(index), format!("line {index}")))
            .collect();
        state.scroll_offset = 10;

        let output = render_snapshot(&state, 100, 12).unwrap();

        assert!(output.contains("line 15"));
        assert!(!output.contains("line 29"));
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
            approval_preview: None,
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
            approval_preview: None,
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
            approval_preview: None,
            diff_preview: Some(format!("--- a/scratch.txt\n+++ b/scratch.txt\n{body}")),
        });

        let output = render_to_text(&state);

        assert!(output.contains("g grant"));
        assert!(output.contains("d deny"));
        assert!(output.contains("diff preview"));
        assert!(output.contains("--- a/scratch.txt"));
    }

    #[test]
    fn renders_approval_modal_approval_preview_when_present() {
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
            tool_name: "shell.exec".into(),
            effect: "ExternalSideEffect".into(),
            reason: "shell.exec requires approval".into(),
            input_preview: r#"{"command":"cargo test"}"#.into(),
            approval_preview: Some("command: cargo test\ncwd: /tmp/work".into()),
            diff_preview: None,
        });

        let output = render_to_text(&state);

        assert!(output.contains("approval preview"));
        assert!(output.contains("command: cargo test"));
        assert!(output.contains("cwd: /tmp/work"));
        assert!(!output.contains("input preview:"));
    }

    fn render_to_text(state: &TuiState) -> String {
        render_snapshot(state, 100, 24).unwrap()
    }
}
