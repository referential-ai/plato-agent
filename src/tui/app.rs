use crate::{
    AppResult,
    daemon::client::{DaemonClient, DaemonConnectionConfig},
    daemon::protocol::{CommandAcceptedResult, EventsStreamResult, RunStartResult},
    tui::{TranscriptState, TranscriptView, TuiState, render, render_snapshot},
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(200);
const EVENT_LIMIT: usize = 32;
const MAX_LIVE_EVENT_LINES: usize = 80;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiOptions {
    pub workspace: PathBuf,
    pub socket: Option<PathBuf>,
    pub run: Option<String>,
    pub config: Option<PathBuf>,
    pub snapshot: bool,
}

impl TuiOptions {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            socket: None,
            run: None,
            config: None,
            snapshot: false,
        }
    }
}

pub fn run_tui(options: TuiOptions) -> AppResult<()> {
    let config = DaemonConnectionConfig::resolve(&options.workspace, options.socket)?;
    let mut state = load_state(&config, options.run.as_deref());
    if options.snapshot {
        print!("{}", render_snapshot(&state, 100, 24)?);
        return Ok(());
    }
    let config_path = options
        .config
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let (commands, events) = spawn_client_worker(config.clone());
    let mut runtime = UiRuntime::from_state(&state);
    let mut terminal = TerminalSession::enter()?;

    loop {
        drain_client_events(&mut state, &mut runtime, &events, &commands);
        maybe_poll_events(&mut runtime, &commands);
        terminal.draw(&state)?;
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && !handle_key_press(
                key,
                &mut state,
                &runtime,
                &commands,
                options.run.clone(),
                config_path.clone(),
            )
        {
            break;
        }
    }
    Ok(())
}

fn handle_key_press(
    key: KeyEvent,
    state: &mut TuiState,
    runtime: &UiRuntime,
    commands: &Sender<ClientCommand>,
    initial_run_id: Option<String>,
    config_path: Option<String>,
) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return request_cancel(commands, state);
    }

    if state.approval.is_some() {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char('g') => decide_approval(commands, state, ApprovalAction::Grant),
            KeyCode::Char('d') => decide_approval(commands, state, ApprovalAction::Deny),
            _ => {}
        }
        return true;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
        state.composer.clear();
        return true;
    }

    match key.code {
        KeyCode::Esc => false,
        KeyCode::Char('q') if state.composer.is_empty() => false,
        KeyCode::Char('r') if is_disconnected(state) => {
            reconnect(commands, state, initial_run_id);
            true
        }
        KeyCode::Enter => {
            submit_composer(commands, state, runtime, config_path);
            true
        }
        KeyCode::Backspace => {
            state.composer.pop();
            true
        }
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            state.composer.push(ch);
            true
        }
        _ => true,
    }
}

fn reconnect(commands: &Sender<ClientCommand>, state: &mut TuiState, run_id: Option<String>) {
    state.status_message = Some("reconnecting".into());
    send_command(commands, ClientCommand::Load { run_id }, state);
}

fn is_disconnected(state: &TuiState) -> bool {
    matches!(
        state.connection,
        crate::tui::ConnectionState::Disconnected { .. }
    )
}

fn load_state(config: &DaemonConnectionConfig, run_id: Option<&str>) -> TuiState {
    match load_connected_state(config, run_id) {
        Ok(state) => state,
        Err(error) => TuiState::disconnected(
            config.workspace_root.to_string_lossy().into_owned(),
            config.socket_path.to_string_lossy().into_owned(),
            error.to_string(),
        ),
    }
}

fn load_connected_state(
    config: &DaemonConnectionConfig,
    run_id: Option<&str>,
) -> AppResult<TuiState> {
    let mut client = DaemonClient::connect(&config.socket_path)?;
    let hello = client.hello(&config.workspace_root)?;
    let sessions = client.sessions_list()?;
    let selected_run = run_id
        .map(str::to_owned)
        .or_else(|| sessions.first().map(|session| session.run_id.clone()));
    let transcript = match selected_run {
        Some(run_id) => match client.transcript_read(&run_id) {
            Ok(transcript) => TranscriptState::Loaded(TranscriptView::from(transcript)),
            Err(error) => TranscriptState::Unavailable {
                run_id,
                error: error.to_string(),
            },
        },
        None => TranscriptState::None,
    };
    let mut state = TuiState::connected(
        config.workspace_root.to_string_lossy().into_owned(),
        config.socket_path.to_string_lossy().into_owned(),
        hello,
        sessions,
        transcript,
    );
    if let Some(session) = state
        .sessions
        .iter()
        .find(|session| session.status == "running")
    {
        state.active_run = Some(crate::tui::ActiveRunView {
            run_id: session.run_id.clone(),
            status: session.status.clone(),
        });
    }
    Ok(state)
}

#[derive(Debug)]
struct UiRuntime {
    active_run_id: Option<String>,
    next_offset: u64,
    poll_in_flight: bool,
    polling: bool,
    last_poll: Instant,
    tool_inputs: HashMap<String, String>,
}

impl UiRuntime {
    fn from_state(state: &TuiState) -> Self {
        Self {
            active_run_id: state.active_run.as_ref().map(|run| run.run_id.clone()),
            next_offset: 0,
            poll_in_flight: false,
            polling: state
                .active_run
                .as_ref()
                .is_some_and(|run| run.status == "running"),
            last_poll: Instant::now(),
            tool_inputs: HashMap::new(),
        }
    }

    fn sync_from_state(&mut self, state: &TuiState) {
        self.active_run_id = state.active_run.as_ref().map(|run| run.run_id.clone());
        self.polling = state
            .active_run
            .as_ref()
            .is_some_and(|run| run.status == "running");
        self.next_offset = 0;
        self.poll_in_flight = false;
        self.last_poll = Instant::now();
        self.tool_inputs.clear();
    }
}

#[derive(Debug)]
enum ClientCommand {
    Load {
        run_id: Option<String>,
    },
    RunStart {
        question: String,
        config_path: Option<String>,
    },
    PollEvents {
        run_id: String,
        from_offset: u64,
    },
    ApprovalGrant {
        run_id: String,
        tool_call_id: String,
    },
    ApprovalDeny {
        run_id: String,
        tool_call_id: String,
        reason: String,
    },
    RunCancel {
        run_id: String,
    },
}

#[derive(Debug)]
enum ClientEvent {
    Loaded(Box<TuiState>),
    RunStarted(RunStartResult),
    EventsPolled(EventsStreamResult),
    ApprovalDecided(CommandAcceptedResult),
    RunCanceled(CommandAcceptedResult),
    Failed {
        context: &'static str,
        error: String,
    },
}

fn spawn_client_worker(
    config: DaemonConnectionConfig,
) -> (Sender<ClientCommand>, Receiver<ClientEvent>) {
    let (command_sender, command_receiver) = mpsc::channel();
    let (event_sender, event_receiver) = mpsc::channel();
    thread::spawn(move || {
        for command in command_receiver {
            let event = handle_client_command(&config, command);
            if event_sender.send(event).is_err() {
                break;
            }
        }
    });
    (command_sender, event_receiver)
}

fn handle_client_command(config: &DaemonConnectionConfig, command: ClientCommand) -> ClientEvent {
    match command {
        ClientCommand::Load { run_id } => {
            ClientEvent::Loaded(Box::new(load_state(config, run_id.as_deref())))
        }
        ClientCommand::RunStart {
            question,
            config_path,
        } => with_client(config, |client| {
            client.run_start(question, config_path, false)
        })
        .map_or_else(failed_event("run.start"), ClientEvent::RunStarted),
        ClientCommand::PollEvents {
            run_id,
            from_offset,
        } => with_client(config, |client| {
            client.events_stream(&run_id, from_offset, EVENT_LIMIT)
        })
        .map_or_else(failed_event("events.stream"), ClientEvent::EventsPolled),
        ClientCommand::ApprovalGrant {
            run_id,
            tool_call_id,
        } => with_client(config, |client| {
            client.approval_grant(&run_id, &tool_call_id)
        })
        .map_or_else(
            failed_event("approval.decide"),
            ClientEvent::ApprovalDecided,
        ),
        ClientCommand::ApprovalDeny {
            run_id,
            tool_call_id,
            reason,
        } => with_client(config, |client| {
            client.approval_deny(&run_id, &tool_call_id, reason)
        })
        .map_or_else(
            failed_event("approval.decide"),
            ClientEvent::ApprovalDecided,
        ),
        ClientCommand::RunCancel { run_id } => {
            with_client(config, |client| client.run_cancel(&run_id))
                .map_or_else(failed_event("run.cancel"), ClientEvent::RunCanceled)
        }
    }
}

fn with_client<T>(
    config: &DaemonConnectionConfig,
    run: impl FnOnce(&mut DaemonClient) -> AppResult<T>,
) -> AppResult<T> {
    let mut client = DaemonClient::connect(&config.socket_path)?;
    client.hello(&config.workspace_root)?;
    run(&mut client)
}

fn failed_event(context: &'static str) -> impl FnOnce(crate::AppError) -> ClientEvent {
    move |error| ClientEvent::Failed {
        context,
        error: error.to_string(),
    }
}

fn drain_client_events(
    state: &mut TuiState,
    runtime: &mut UiRuntime,
    events: &Receiver<ClientEvent>,
    commands: &Sender<ClientCommand>,
) {
    while let Ok(event) = events.try_recv() {
        match event {
            ClientEvent::Loaded(loaded) => {
                apply_loaded_state(state, *loaded);
                runtime.sync_from_state(state);
            }
            ClientEvent::RunStarted(result) => {
                apply_run_response(state, runtime, result, "run started")
            }
            ClientEvent::EventsPolled(result) => {
                apply_events_result(state, runtime, commands, result)
            }
            ClientEvent::ApprovalDecided(result) => {
                state.status_message =
                    Some(format!("approval decision sent for {}", result.run_id));
                state.approval = None;
                state.active_run = Some(crate::tui::ActiveRunView {
                    run_id: result.run_id,
                    status: result.status,
                });
            }
            ClientEvent::RunCanceled(result) => {
                state.status_message = Some(format!("cancel requested for {}", result.run_id));
                state.cancel_requested = true;
                state.approval = None;
                state.active_run = Some(crate::tui::ActiveRunView {
                    run_id: result.run_id.clone(),
                    status: result.status,
                });
                push_live_event(state, None, format!("cancel requested: {}", result.run_id));
            }
            ClientEvent::Failed { context, error } => {
                runtime.poll_in_flight = false;
                if context == "events.stream" && error.starts_with("lagged:") {
                    state.stream_warning = Some(format!("{error}; transcript refresh requested"));
                    if let Some(run_id) = &runtime.active_run_id {
                        send_command(
                            commands,
                            ClientCommand::Load {
                                run_id: Some(run_id.clone()),
                            },
                            state,
                        );
                    }
                } else if context == "events.stream" && error.starts_with("overload:") {
                    state.stream_warning = Some(error);
                } else {
                    if is_connection_error(&error) {
                        runtime.polling = false;
                        state.connection = crate::tui::ConnectionState::Disconnected {
                            error: error.clone(),
                        };
                    }
                    if context == "run.cancel" {
                        state.cancel_requested = false;
                    }
                    state.status_message = Some(format!("{context} failed: {error}"));
                }
            }
        }
    }
}

fn apply_loaded_state(state: &mut TuiState, mut loaded: TuiState) {
    loaded.composer = std::mem::take(&mut state.composer);
    if loaded.status_message.is_none() {
        loaded.status_message = state.status_message.clone();
    }
    if loaded.stream_warning.is_none() {
        loaded.stream_warning = state.stream_warning.clone();
    }
    if loaded.live_events.is_empty() {
        loaded.live_events = state.live_events.clone();
    }
    if loaded.active_run.as_ref().map(|run| &run.run_id)
        == state.active_run.as_ref().map(|run| &run.run_id)
    {
        loaded.cancel_requested = state.cancel_requested;
    }
    *state = loaded;
}

fn apply_run_response(
    state: &mut TuiState,
    runtime: &mut UiRuntime,
    result: RunStartResult,
    message: &'static str,
) {
    let run_id = result.run_id.clone();
    let status = result.status.clone();
    state.status_message = Some(format!("{message}: {run_id}"));
    state.stream_warning = None;
    state.cancel_requested = false;
    state.approval = None;
    state.active_run = Some(crate::tui::ActiveRunView {
        run_id: run_id.clone(),
        status: status.clone(),
    });
    push_live_event(state, None, format!("{message}: {run_id}"));
    runtime.active_run_id = Some(run_id);
    runtime.next_offset = 0;
    runtime.poll_in_flight = false;
    runtime.polling = status == "running";
    runtime.last_poll = Instant::now() - ACTIVE_POLL_INTERVAL;
    runtime.tool_inputs.clear();
}

fn apply_events_result(
    state: &mut TuiState,
    runtime: &mut UiRuntime,
    commands: &Sender<ClientCommand>,
    result: EventsStreamResult,
) {
    runtime.poll_in_flight = false;
    runtime.next_offset = result.next_offset;
    runtime.polling = result.status == "running";
    state.stream_warning = None;
    state.active_run = Some(crate::tui::ActiveRunView {
        run_id: result.run_id.clone(),
        status: result.status.clone(),
    });
    for event in result.events {
        if let Some((call_id, input_preview)) = crate::tui::tool_input_preview_from_event(&event) {
            runtime
                .tool_inputs
                .insert(call_id.clone(), input_preview.clone());
            if let Some(approval) = state.approval.as_mut()
                && approval.tool_call_id == call_id
            {
                approval.input_preview = input_preview;
            }
        }
        if let Some(approval) = crate::tui::approval_from_event(
            &event,
            event
                .get("event")
                .and_then(|event| event.get("tool_call_id"))
                .and_then(|call_id| call_id.as_str())
                .and_then(|call_id| runtime.tool_inputs.get(call_id).cloned()),
        ) {
            state.approval = Some(approval);
        }
        let line = crate::tui::live_event_line(&event);
        push_live_event(state, line.offset, line.text);
    }
    if result.status != "running" {
        send_command(
            commands,
            ClientCommand::Load {
                run_id: Some(result.run_id),
            },
            state,
        );
    }
}

fn maybe_poll_events(runtime: &mut UiRuntime, commands: &Sender<ClientCommand>) {
    if !runtime.polling || runtime.poll_in_flight {
        return;
    }
    if runtime.last_poll.elapsed() < ACTIVE_POLL_INTERVAL {
        return;
    }
    let Some(run_id) = runtime.active_run_id.clone() else {
        return;
    };
    if commands
        .send(ClientCommand::PollEvents {
            run_id,
            from_offset: runtime.next_offset,
        })
        .is_ok()
    {
        runtime.poll_in_flight = true;
        runtime.last_poll = Instant::now();
    } else {
        runtime.polling = false;
    }
}

enum ApprovalAction {
    Grant,
    Deny,
}

fn decide_approval(commands: &Sender<ClientCommand>, state: &mut TuiState, action: ApprovalAction) {
    let Some(approval) = state.approval.clone() else {
        return;
    };
    let command = match action {
        ApprovalAction::Grant => ClientCommand::ApprovalGrant {
            run_id: approval.run_id.clone(),
            tool_call_id: approval.tool_call_id.clone(),
        },
        ApprovalAction::Deny => ClientCommand::ApprovalDeny {
            run_id: approval.run_id.clone(),
            tool_call_id: approval.tool_call_id.clone(),
            reason: "denied by plato-tui".into(),
        },
    };
    state.status_message = Some(match action {
        ApprovalAction::Grant => format!("grant sent for {}", approval.tool_call_id),
        ApprovalAction::Deny => format!("deny sent for {}", approval.tool_call_id),
    });
    state.approval = None;
    send_command(commands, command, state);
}

fn request_cancel(commands: &Sender<ClientCommand>, state: &mut TuiState) -> bool {
    let Some(active) = state.active_run.clone() else {
        return false;
    };
    if active.status != "running" || state.cancel_requested {
        return false;
    }
    state.cancel_requested = true;
    state.status_message = Some(format!("cancel requested for {}", active.run_id));
    send_command(
        commands,
        ClientCommand::RunCancel {
            run_id: active.run_id,
        },
        state,
    );
    true
}

fn is_connection_error(error: &str) -> bool {
    error.contains("Connection refused")
        || error.contains("No such file")
        || error.contains("connection closed")
        || error.contains("unsupported_version")
        || error.contains("workspace_mismatch")
        || error.contains("DaemonLockHeld")
}

fn submit_composer(
    commands: &Sender<ClientCommand>,
    state: &mut TuiState,
    runtime: &UiRuntime,
    config_path: Option<String>,
) {
    let message = state.composer.trim().to_string();
    if message.is_empty() {
        return;
    }
    if runtime.polling && runtime.active_run_id.is_some() {
        state.status_message = Some("run already active".into());
        return;
    }
    state.composer.clear();
    let command = ClientCommand::RunStart {
        question: message,
        config_path,
    };
    state.status_message = Some("submitted to daemon".into());
    send_command(commands, command, state);
}

fn send_command(commands: &Sender<ClientCommand>, command: ClientCommand, state: &mut TuiState) {
    if commands.send(command).is_err() {
        state.status_message = Some("daemon client worker stopped".into());
    }
}

fn push_live_event(state: &mut TuiState, offset: Option<u64>, text: impl Into<String>) {
    state
        .live_events
        .push(crate::tui::LiveEventLine::new(offset, text));
    if state.live_events.len() > MAX_LIVE_EVENT_LINES {
        let excess = state.live_events.len() - MAX_LIVE_EVENT_LINES;
        state.live_events.drain(0..excess);
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> AppResult<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }

    fn draw(&mut self, state: &TuiState) -> AppResult<()> {
        self.terminal.draw(|frame| render(frame, state))?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{daemon::protocol::HelloResult, tui::TranscriptState};
    use serde_json::json;

    #[test]
    fn submit_composer_uses_run_start_when_idle() {
        let (sender, receiver) = mpsc::channel();
        let mut state = test_state();
        state.composer = "start work".into();
        let runtime = UiRuntime::from_state(&state);

        submit_composer(&sender, &mut state, &runtime, Some("plato.toml".into()));

        match receiver.try_recv().unwrap() {
            ClientCommand::RunStart {
                question,
                config_path,
            } => {
                assert_eq!(question, "start work");
                assert_eq!(config_path.as_deref(), Some("plato.toml"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(state.composer.is_empty());
    }

    #[test]
    fn submit_composer_rejects_follow_up_while_run_is_polling() {
        let (sender, receiver) = mpsc::channel();
        let mut state = test_state();
        state.composer = "next turn".into();
        let runtime = UiRuntime {
            active_run_id: Some("run_1".into()),
            next_offset: 0,
            poll_in_flight: false,
            polling: true,
            last_poll: Instant::now(),
            tool_inputs: HashMap::new(),
        };

        submit_composer(&sender, &mut state, &runtime, None);

        assert!(receiver.try_recv().is_err());
        assert_eq!(state.composer, "next turn");
        assert_eq!(state.status_message.as_deref(), Some("run already active"));
    }

    #[test]
    fn printable_r_is_composer_text_when_connected() {
        let (sender, receiver) = mpsc::channel();
        let mut state = test_state();
        let runtime = UiRuntime::from_state(&state);

        for ch in "read write current target/current".chars() {
            assert!(handle_key_press(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
                &mut state,
                &runtime,
                &sender,
                None,
                None,
            ));
        }

        assert_eq!(state.composer, "read write current target/current");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn r_reconnects_from_disconnected_state() {
        let (sender, receiver) = mpsc::channel();
        let mut state = test_state();
        state.connection = crate::tui::ConnectionState::Disconnected {
            error: "connection closed".into(),
        };
        let runtime = UiRuntime::from_state(&state);

        assert!(handle_key_press(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::empty()),
            &mut state,
            &runtime,
            &sender,
            Some("run_1".into()),
            None,
        ));

        assert_eq!(state.status_message.as_deref(), Some("reconnecting"));
        match receiver.try_recv().unwrap() {
            ClientCommand::Load { run_id } => assert_eq!(run_id.as_deref(), Some("run_1")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn events_result_updates_live_state_and_requests_reload_on_finish() {
        let (sender, receiver) = mpsc::channel();
        let mut state = test_state();
        let mut runtime = UiRuntime {
            active_run_id: Some("run_1".into()),
            next_offset: 0,
            poll_in_flight: true,
            polling: true,
            last_poll: Instant::now(),
            tool_inputs: HashMap::new(),
        };
        let result = EventsStreamResult {
            run_id: "run_1".into(),
            from_offset: 0,
            next_offset: 2,
            status: "finished".into(),
            events: vec![json!({
                "offset": 1,
                "event": {
                    "kind": "ledger",
                    "record": {
                        "event": {
                            "event": "run_finished"
                        }
                    }
                }
            })],
        };

        apply_events_result(&mut state, &mut runtime, &sender, result);

        assert_eq!(runtime.next_offset, 2);
        assert!(!runtime.polling);
        assert_eq!(state.live_events[0].text, "run finished");
        match receiver.try_recv().unwrap() {
            ClientCommand::Load { run_id } => assert_eq!(run_id.as_deref(), Some("run_1")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn stream_connection_failure_enters_disconnected_and_stops_polling() {
        let (command_sender, command_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        let mut state = test_state();
        state.active_run = Some(crate::tui::ActiveRunView {
            run_id: "run_1".into(),
            status: "running".into(),
        });
        let mut runtime = UiRuntime {
            active_run_id: Some("run_1".into()),
            next_offset: 7,
            poll_in_flight: true,
            polling: true,
            last_poll: Instant::now() - ACTIVE_POLL_INTERVAL,
            tool_inputs: HashMap::new(),
        };
        event_sender
            .send(ClientEvent::Failed {
                context: "events.stream",
                error: "io error: Connection refused (os error 111)".into(),
            })
            .unwrap();

        drain_client_events(&mut state, &mut runtime, &event_receiver, &command_sender);
        maybe_poll_events(&mut runtime, &command_sender);

        assert!(!runtime.polling);
        assert!(!runtime.poll_in_flight);
        assert!(is_disconnected(&state));
        assert!(command_receiver.try_recv().is_err());
    }

    #[test]
    fn approval_preview_updates_when_tool_input_arrives_after_request() {
        let (sender, _receiver) = mpsc::channel();
        let mut state = test_state();
        let mut runtime = UiRuntime {
            active_run_id: Some("run_1".into()),
            next_offset: 0,
            poll_in_flight: true,
            polling: true,
            last_poll: Instant::now(),
            tool_inputs: HashMap::new(),
        };
        let result = EventsStreamResult {
            run_id: "run_1".into(),
            from_offset: 0,
            next_offset: 2,
            status: "running".into(),
            events: vec![
                json!({
                    "offset": 1,
                    "event": {
                        "kind": "approval_requested",
                        "run_id": "run_1",
                        "tool_call_id": "call_1",
                        "tool_name": "file.write",
                        "effect": "WorkspaceWrite",
                        "reason": "file.write requires approval"
                    }
                }),
                json!({
                    "offset": 2,
                    "event": {
                        "kind": "ledger",
                        "record": {
                            "event": {
                                "event": "tool_call_proposed",
                                "call": {
                                    "id": "call_1",
                                    "tool": "file.write",
                                    "effect": "WorkspaceWrite",
                                    "input": {
                                        "path": "scratch/tui-preview.txt",
                                        "content": "preview body"
                                    }
                                }
                            }
                        }
                    }
                }),
            ],
        };

        apply_events_result(&mut state, &mut runtime, &sender, result);

        let approval = state.approval.as_ref().expect("approval modal");
        assert_eq!(approval.tool_call_id, "call_1");
        assert!(approval.input_preview.contains("scratch/tui-preview.txt"));
        assert!(approval.input_preview.contains("preview body"));
    }

    #[test]
    fn approval_decisions_send_daemon_commands() {
        let (sender, receiver) = mpsc::channel();
        let mut state = test_state();
        state.approval = Some(crate::tui::ApprovalModalView {
            run_id: "run_1".into(),
            tool_call_id: "call_1".into(),
            tool_name: "file.write".into(),
            effect: "WorkspaceWrite".into(),
            reason: "requires approval".into(),
            input_preview: "{}".into(),
            diff_preview: None,
        });

        decide_approval(&sender, &mut state, ApprovalAction::Grant);

        assert!(state.approval.is_none());
        match receiver.try_recv().unwrap() {
            ClientCommand::ApprovalGrant {
                run_id,
                tool_call_id,
            } => {
                assert_eq!(run_id, "run_1");
                assert_eq!(tool_call_id, "call_1");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        state.approval = Some(crate::tui::ApprovalModalView {
            run_id: "run_2".into(),
            tool_call_id: "call_2".into(),
            tool_name: "file.write".into(),
            effect: "WorkspaceWrite".into(),
            reason: "requires approval".into(),
            input_preview: "{}".into(),
            diff_preview: None,
        });

        decide_approval(&sender, &mut state, ApprovalAction::Deny);

        match receiver.try_recv().unwrap() {
            ClientCommand::ApprovalDeny {
                run_id,
                tool_call_id,
                reason,
            } => {
                assert_eq!(run_id, "run_2");
                assert_eq!(tool_call_id, "call_2");
                assert_eq!(reason, "denied by plato-tui");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn first_cancel_requests_daemon_and_second_cancel_quits() {
        let (sender, receiver) = mpsc::channel();
        let mut state = test_state();
        state.active_run = Some(crate::tui::ActiveRunView {
            run_id: "run_1".into(),
            status: "running".into(),
        });

        assert!(request_cancel(&sender, &mut state));
        assert!(state.cancel_requested);
        match receiver.try_recv().unwrap() {
            ClientCommand::RunCancel { run_id } => assert_eq!(run_id, "run_1"),
            other => panic!("unexpected command: {other:?}"),
        }

        assert!(!request_cancel(&sender, &mut state));
    }

    fn test_state() -> TuiState {
        TuiState::connected(
            "/tmp/workspace".into(),
            "/tmp/agent.sock".into(),
            HelloResult {
                daemon_version: "0.1.0".into(),
                workspace_id: "workspace-1234".into(),
                ledger_path: "/tmp/agent.db".into(),
                capabilities: vec![],
            },
            Vec::new(),
            TranscriptState::None,
        )
    }
}
