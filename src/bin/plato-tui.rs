use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use plato_agent::{
    AppResult,
    daemon::client::{DaemonClient, DaemonConnectionConfig},
    tui::{TranscriptState, TranscriptView, TuiState, render, render_snapshot},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, Stdout},
    path::PathBuf,
    time::Duration,
};

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
    let mut terminal = TerminalSession::enter()?;

    loop {
        terminal.draw(&state)?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Char('r') => state = load_state(&config, cli.run.as_deref()),
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
    Ok(TuiState::connected(
        config.workspace_root.to_string_lossy().into_owned(),
        config.socket_path.to_string_lossy().into_owned(),
        hello,
        sessions,
        transcript,
    ))
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
