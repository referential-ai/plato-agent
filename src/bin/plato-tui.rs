use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use plato_agent::{
    AppResult,
    daemon::client::{DaemonClient, DaemonConnectionConfig},
    daemon::protocol::{EventsStreamResult, RunStartResult},
    tui::{TranscriptState, TranscriptView, TuiState, render, render_snapshot},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, Stdout},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(200);
const EVENT_LIMIT: usize = 32;
const MAX_LIVE_EVENT_LINES: usize = 80;

#[derive(Debug, Parser)]
#[command(name = "plato-tui")]
#[command(about = "Plato Agent terminal client")]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,

    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    #[arg(long, value_name = "RUN_ID")]
    run: Option<String>,

    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    #[arg(long)]
    snapshot: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> AppResult<()> {
    let cli = Cli::parse();
    let config = DaemonConnectionConfig::resolve(&cli.workspace, cli.socket)?;
    let mut state = load_state(&config, cli.run.as_deref());
    if cli.snapshot {
        print!("{}", render_snapshot(&state, 100, 24)?);
        return Ok(());
    }
    let config_path = cli
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
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Char('r') => {
                    state.status_message = Some("reconnecting".into());
                    send_command(
                        &commands,
                        ClientCommand::Load {
                            run_id: cli.run.clone(),
                        },
                        &mut state,
                    );
                }
                KeyCode::Enter => {
                    submit_composer(&commands, &mut state, &runtime, config_path.clone())
                }
                KeyCode::Backspace => {
                    state.composer.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    state.composer.clear();
                }
                KeyCode::Char(ch) => state.composer.push(ch),
                _ => {}
            }
        }
    }
    Ok(())
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
        state.active_run = Some(plato_agent::tui::ActiveRunView {
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
    MessageAppend {
        message: String,
        config_path: Option<String>,
    },
    PollEvents {
        run_id: String,
        from_offset: u64,
    },
}

#[derive(Debug)]
enum ClientEvent {
    Loaded(Box<TuiState>),
    RunStarted(RunStartResult),
    MessageAppended(RunStartResult),
    EventsPolled(EventsStreamResult),
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
        ClientCommand::MessageAppend {
            message,
            config_path,
        } => with_client(config, |client| {
            client.message_append(message, config_path, false)
        })
        .map_or_else(failed_event("message.append"), ClientEvent::MessageAppended),
        ClientCommand::PollEvents {
            run_id,
            from_offset,
        } => with_client(config, |client| {
            client.events_stream(&run_id, from_offset, EVENT_LIMIT)
        })
        .map_or_else(failed_event("events.stream"), ClientEvent::EventsPolled),
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

fn failed_event(context: &'static str) -> impl FnOnce(plato_agent::AppError) -> ClientEvent {
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
            ClientEvent::MessageAppended(result) => {
                apply_run_response(state, runtime, result, "message appended")
            }
            ClientEvent::EventsPolled(result) => {
                apply_events_result(state, runtime, commands, result)
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
    state.active_run = Some(plato_agent::tui::ActiveRunView {
        run_id: run_id.clone(),
        status: status.clone(),
    });
    push_live_event(state, None, format!("{message}: {run_id}"));
    runtime.active_run_id = Some(run_id);
    runtime.next_offset = 0;
    runtime.poll_in_flight = false;
    runtime.polling = status == "running";
    runtime.last_poll = Instant::now() - ACTIVE_POLL_INTERVAL;
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
    state.active_run = Some(plato_agent::tui::ActiveRunView {
        run_id: result.run_id.clone(),
        status: result.status.clone(),
    });
    for event in result.events {
        let line = plato_agent::tui::live_event_line(&event);
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
    state.composer.clear();
    let command = if runtime.polling && runtime.active_run_id.is_some() {
        ClientCommand::MessageAppend {
            message,
            config_path,
        }
    } else {
        ClientCommand::RunStart {
            question: message,
            config_path,
        }
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
        .push(plato_agent::tui::LiveEventLine::new(offset, text));
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
    use plato_agent::{daemon::protocol::HelloResult, tui::TranscriptState};
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
    fn submit_composer_uses_message_append_while_run_is_polling() {
        let (sender, receiver) = mpsc::channel();
        let mut state = test_state();
        state.composer = "next turn".into();
        let runtime = UiRuntime {
            active_run_id: Some("run_1".into()),
            next_offset: 0,
            poll_in_flight: false,
            polling: true,
            last_poll: Instant::now(),
        };

        submit_composer(&sender, &mut state, &runtime, None);

        match receiver.try_recv().unwrap() {
            ClientCommand::MessageAppend {
                message,
                config_path,
            } => {
                assert_eq!(message, "next turn");
                assert_eq!(config_path, None);
            }
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
