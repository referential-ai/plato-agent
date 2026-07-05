use clap::{Parser, Subcommand};
use plato_agent::{
    AppError, ApprovalMode, RunLedger, RunOptions, RunOutcome, RunSession,
    daemon::{
        client::{DaemonClient, DaemonConnectionConfig},
        lock::ensure_workspace_unlocked,
        server::DaemonServer,
    },
    ledger::latest_sqlite_session_id,
    new_session_id,
    paths::default_sqlite_path,
    replay_file, replay_sqlite, run_question,
    tui::{TuiOptions, run_tui},
};
use platonic_core::RunId;
use std::{
    io::{self, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const EMBEDDED_DAEMON_TIMEOUT: Duration = Duration::from_secs(3);
const EMBEDDED_DAEMON_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Parser)]
#[command(name = "plato")]
#[command(about = "Plato Agent CLI")]
struct Cli {
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[arg(long, value_name = "FILE")]
    events: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        value_name = "PATH",
        num_args = 0..=1,
        require_equals = true,
        help = "Use SQLite ledger; bare --db uses the default XDG state path"
    )]
    db: Option<Option<PathBuf>>,

    #[arg(
        long,
        global = true,
        help = "Auto-approve enabled tool calls that would otherwise prompt"
    )]
    yolo: bool,

    #[arg(
        short = 'c',
        long = "continue",
        help = "Continue the latest SQLite workspace session"
    )]
    continue_session: bool,

    #[arg(long, global = true, help = "Start the interactive terminal UI")]
    tui: bool,

    #[command(subcommand)]
    command: Option<Command>,

    #[arg(value_name = "QUESTION")]
    question: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Replay {
        #[arg(long, value_name = "RUN_ID")]
        run: Option<String>,

        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> plato_agent::AppResult<()> {
    let cli = Cli::parse();
    let workspace_root = std::env::current_dir()?;
    if cli.tui {
        return run_tui_mode(cli, workspace_root);
    }
    match cli.command {
        Some(Command::Replay { run, file }) => {
            let ledger = replay_ledger(cli.db, file, &workspace_root)?;
            write_replay_output(&mut io::stdout(), ledger, run.as_deref(), &workspace_root)
        }
        None => {
            let question = cli.question.join(" ");
            let ledger = run_ledger(cli.events, cli.db, &workspace_root)?;
            let session = run_session(cli.continue_session, &ledger)?;
            let outcome = run_question(RunOptions {
                question,
                config_path: cli.config,
                ledger: ledger.clone(),
                workspace_root,
                approval_mode: ApprovalMode::from_yolo(cli.yolo),
                run_id: None,
                session,
                event_sender: None,
                cancel: None,
            })?;
            write_run_success_output(&mut io::stdout(), &mut io::stderr(), &outcome, &ledger)
        }
    }
}

fn run_tui_mode(cli: Cli, workspace_root: PathBuf) -> plato_agent::AppResult<()> {
    let options = tui_options_from_cli(&cli, &workspace_root)?;
    let _embedded_daemon = ensure_tui_daemon(&workspace_root)?;
    run_tui(options)
}

fn tui_options_from_cli(cli: &Cli, workspace_root: &Path) -> plato_agent::AppResult<TuiOptions> {
    validate_tui_cli(cli)?;
    let mut options = TuiOptions::new(workspace_root.to_path_buf());
    options.config = cli.config.clone();
    Ok(options)
}

fn validate_tui_cli(cli: &Cli) -> plato_agent::AppResult<()> {
    if cli.command.is_some() {
        return Err(AppError::Config(
            "plato --tui cannot be combined with subcommands".into(),
        ));
    }
    if !cli.question.is_empty() {
        return Err(AppError::Config(
            "plato --tui cannot be combined with a question".into(),
        ));
    }
    if cli.events.is_some() || cli.db.is_some() || cli.yolo || cli.continue_session {
        return Err(AppError::Config(
            "plato --tui cannot be combined with --events, --db, --yolo, or -c".into(),
        ));
    }
    Ok(())
}

fn ensure_tui_daemon(workspace_root: &Path) -> plato_agent::AppResult<Option<EmbeddedDaemon>> {
    let config = DaemonConnectionConfig::resolve(workspace_root, None)?;
    if daemon_accepts_hello(&config) {
        return Ok(None);
    }
    start_embedded_daemon(workspace_root, &config).map(Some)
}

fn start_embedded_daemon(
    workspace_root: &Path,
    config: &DaemonConnectionConfig,
) -> plato_agent::AppResult<EmbeddedDaemon> {
    let server = DaemonServer::bind(workspace_root, None)?;
    let socket_path = server.paths().socket_path.clone();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = shutdown.clone();
    let handle = thread::spawn(move || server.serve_forever(thread_shutdown));
    let mut daemon = EmbeddedDaemon {
        shutdown,
        socket_path,
        handle: Some(handle),
    };
    wait_for_embedded_daemon(config, &mut daemon)?;
    Ok(daemon)
}

fn wait_for_embedded_daemon(
    config: &DaemonConnectionConfig,
    daemon: &mut EmbeddedDaemon,
) -> plato_agent::AppResult<()> {
    let deadline = Instant::now() + EMBEDDED_DAEMON_TIMEOUT;
    loop {
        if daemon_accepts_hello(config) {
            return Ok(());
        }
        if daemon.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            return daemon_finished_before_ready(daemon);
        }
        if Instant::now() >= deadline {
            return Err(AppError::Config(format!(
                "timed out waiting for embedded plato-agentd at {}",
                config.socket_path.display()
            )));
        }
        thread::sleep(EMBEDDED_DAEMON_POLL);
    }
}

fn daemon_accepts_hello(config: &DaemonConnectionConfig) -> bool {
    let Ok(mut client) = DaemonClient::connect(&config.socket_path) else {
        return false;
    };
    client.hello(&config.workspace_root).is_ok()
}

fn daemon_finished_before_ready(daemon: &mut EmbeddedDaemon) -> plato_agent::AppResult<()> {
    let Some(handle) = daemon.handle.take() else {
        return Err(AppError::Config(
            "embedded plato-agentd stopped before accepting connections".into(),
        ));
    };
    match handle.join() {
        Ok(Ok(())) => Err(AppError::Config(
            "embedded plato-agentd exited before accepting connections".into(),
        )),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(AppError::Config(
            "embedded plato-agentd panicked before accepting connections".into(),
        )),
    }
}

struct EmbeddedDaemon {
    shutdown: Arc<AtomicBool>,
    socket_path: PathBuf,
    handle: Option<JoinHandle<plato_agent::AppResult<()>>>,
}

impl Drop for EmbeddedDaemon {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_ledger(
    events: Option<PathBuf>,
    db: Option<Option<PathBuf>>,
    workspace_root: &Path,
) -> plato_agent::AppResult<RunLedger> {
    if db.is_some() && events.is_some() {
        return Err(AppError::Config(
            "--events and --db are mutually exclusive".into(),
        ));
    }
    match events {
        Some(path) => Ok(RunLedger::Jsonl(path)),
        None => {
            let path = sqlite_path(db, workspace_root)?;
            ensure_workspace_unlocked(workspace_root)?;
            Ok(RunLedger::Sqlite(path))
        }
    }
}

fn sqlite_path(
    db: Option<Option<PathBuf>>,
    workspace_root: &Path,
) -> plato_agent::AppResult<PathBuf> {
    match db {
        None | Some(None) => default_sqlite_path(workspace_root),
        Some(Some(path)) => Ok(resolve_cli_path(path, workspace_root)),
    }
}

fn run_session(
    continue_session: bool,
    ledger: &RunLedger,
) -> plato_agent::AppResult<Option<RunSession>> {
    match ledger {
        RunLedger::Jsonl(_) if continue_session => Err(AppError::Config(
            "plato -c requires the SQLite ledger; remove --events".into(),
        )),
        RunLedger::Jsonl(_) => Ok(None),
        RunLedger::Sqlite(path) if continue_session => {
            let session_id = latest_sqlite_session_id(path).map_err(|error| match error {
                AppError::NoSqliteSessions | AppError::NoSqliteRuns => AppError::Config(
                    "plato -c found no previous SQLite session; run plato \"...\" first".into(),
                ),
                error => error,
            })?;
            Ok(Some(RunSession::Continue { session_id }))
        }
        RunLedger::Sqlite(_) => Ok(Some(RunSession::Fresh {
            session_id: new_session_id(),
        })),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReplayLedger {
    Jsonl(PathBuf),
    Sqlite(PathBuf),
}

fn replay_ledger(
    db: Option<Option<PathBuf>>,
    file: Option<PathBuf>,
    workspace_root: &Path,
) -> plato_agent::AppResult<ReplayLedger> {
    match (db, file) {
        (Some(_), Some(_)) => Err(AppError::Config(
            "replay accepts either --db or a JSONL file, not both".into(),
        )),
        (None, Some(file)) => Ok(ReplayLedger::Jsonl(file)),
        (db, None) => sqlite_path(db, workspace_root).map(ReplayLedger::Sqlite),
    }
}

fn resolve_cli_path(path: PathBuf, workspace_root: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    }
}

fn write_run_success_output(
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    outcome: &RunOutcome,
    ledger: &RunLedger,
) -> plato_agent::AppResult<()> {
    writeln!(stdout, "{}", outcome.final_answer)?;
    if let RunLedger::Sqlite(path) = ledger {
        write_sqlite_replay_hint(stderr, &outcome.run_id, path)?;
    }
    Ok(())
}

fn write_replay_output(
    stdout: &mut impl Write,
    ledger: ReplayLedger,
    run: Option<&str>,
    workspace_root: &Path,
) -> plato_agent::AppResult<()> {
    match ledger {
        ReplayLedger::Sqlite(path) => {
            ensure_workspace_unlocked(workspace_root)?;
            writeln!(stdout, "{}", replay_sqlite(&path, run)?)?;
        }
        ReplayLedger::Jsonl(file) => {
            if run.is_some() {
                return Err(AppError::Config("replay --run requires --db".into()));
            }
            writeln!(stdout, "{}", replay_file(&file)?)?;
        }
    }
    Ok(())
}

fn write_sqlite_replay_hint(
    stderr: &mut impl Write,
    run_id: &RunId,
    path: &Path,
) -> plato_agent::AppResult<()> {
    let path = path.to_string_lossy();
    writeln!(stderr, "run_id: {run_id}")?;
    writeln!(stderr, "ledger_path: {path}")?;
    writeln!(
        stderr,
        "replay: plato replay --db={} --run {run_id}",
        shell_quote(&path)
    )?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_./:-".contains(character))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_success_hint_goes_to_stderr_without_changing_stdout() {
        let outcome = RunOutcome {
            run_id: RunId::new("run_1").unwrap(),
            final_answer: "done".into(),
        };
        let ledger = RunLedger::Sqlite(PathBuf::from("/tmp/plato proof/agent.db"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        write_run_success_output(&mut stdout, &mut stderr, &outcome, &ledger).unwrap();

        assert_eq!(String::from_utf8(stdout).unwrap(), "done\n");
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "run_id: run_1\nledger_path: /tmp/plato proof/agent.db\nreplay: plato replay --db='/tmp/plato proof/agent.db' --run run_1\n"
        );
    }

    #[test]
    fn jsonl_success_does_not_print_replay_hint() {
        let outcome = RunOutcome {
            run_id: RunId::new("run_1").unwrap(),
            final_answer: "done".into(),
        };
        let ledger = RunLedger::Jsonl(PathBuf::from("events.jsonl"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        write_run_success_output(&mut stdout, &mut stderr, &outcome, &ledger).unwrap();

        assert_eq!(String::from_utf8(stdout).unwrap(), "done\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn tui_flag_builds_tui_options_with_config() {
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from(["plato", "--tui", "--config", "custom.toml"]).unwrap();

        let options = tui_options_from_cli(&cli, dir.path()).unwrap();

        assert_eq!(options.workspace, dir.path());
        assert_eq!(options.config.as_deref(), Some(Path::new("custom.toml")));
        assert_eq!(options.socket, None);
        assert_eq!(options.run, None);
        assert!(!options.snapshot);
    }

    #[test]
    fn tui_flag_rejects_one_shot_only_options() {
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from(["plato", "--tui", "--yolo"]).unwrap();

        let error = tui_options_from_cli(&cli, dir.path()).unwrap_err();

        assert!(matches!(
            error,
            AppError::Config(message)
                if message == "plato --tui cannot be combined with --events, --db, --yolo, or -c"
        ));
    }

    #[test]
    fn tui_flag_rejects_questions() {
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from(["plato", "--tui", "hello"]).unwrap();

        let error = tui_options_from_cli(&cli, dir.path()).unwrap_err();

        assert!(matches!(
            error,
            AppError::Config(message) if message == "plato --tui cannot be combined with a question"
        ));
    }

    #[test]
    fn explicit_sqlite_path_is_resolved_against_workspace_root() {
        let dir = tempfile::tempdir().unwrap();

        let path = sqlite_path(Some(Some(PathBuf::from("agent.db"))), dir.path()).unwrap();

        assert_eq!(path, dir.path().join("agent.db"));
    }

    #[test]
    fn default_run_uses_default_sqlite_path() {
        let workspace = tempfile::tempdir().unwrap();

        let ledger = run_ledger(None, None, workspace.path()).unwrap();

        assert_eq!(
            ledger,
            RunLedger::Sqlite(default_sqlite_path(workspace.path()).unwrap())
        );
    }

    #[test]
    fn default_sqlite_run_fails_closed_when_daemon_lock_exists() {
        let workspace = tempfile::tempdir().unwrap();
        let socket = workspace.path().join("agent.sock");
        let _lock = plato_agent::daemon::lock::WorkspaceLock::acquire_for_workspace(
            workspace.path(),
            &socket,
        )
        .unwrap();

        let error = run_ledger(None, None, workspace.path()).unwrap_err();

        assert!(matches!(error, AppError::DaemonLockHeld { .. }));
    }

    #[test]
    fn jsonl_run_does_not_check_daemon_lock() {
        let workspace = tempfile::tempdir().unwrap();
        let socket = workspace.path().join("agent.sock");
        let _lock = plato_agent::daemon::lock::WorkspaceLock::acquire_for_workspace(
            workspace.path(),
            &socket,
        )
        .unwrap();

        let ledger =
            run_ledger(Some(PathBuf::from("events.jsonl")), None, workspace.path()).unwrap();

        assert_eq!(ledger, RunLedger::Jsonl(PathBuf::from("events.jsonl")));
    }

    #[test]
    fn default_sqlite_run_starts_fresh_session() {
        let workspace = tempfile::tempdir().unwrap();
        let ledger = RunLedger::Sqlite(default_sqlite_path(workspace.path()).unwrap());

        let session = run_session(false, &ledger).unwrap().unwrap();

        assert!(matches!(session, RunSession::Fresh { .. }));
    }

    #[test]
    fn continue_rejects_jsonl_ledger() {
        let ledger = RunLedger::Jsonl(PathBuf::from("events.jsonl"));

        let error = run_session(true, &ledger).unwrap_err();

        assert!(matches!(
            error,
            AppError::Config(message)
                if message == "plato -c requires the SQLite ledger; remove --events"
        ));
    }

    #[test]
    fn continue_uses_latest_sqlite_session() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("agent.db");
        let mut ledger = plato_agent::ledger::SqliteLedger::open_or_create(&path).unwrap();
        let run_id = RunId::new("run_1").unwrap();
        ledger
            .begin_session_run("session_1", &run_id, "hello", true)
            .unwrap();
        ledger.finish_session_run(&run_id, "hi").unwrap();

        let session = run_session(true, &RunLedger::Sqlite(path))
            .unwrap()
            .unwrap();

        assert_eq!(
            session,
            RunSession::Continue {
                session_id: "session_1".into()
            }
        );
    }

    #[test]
    fn bare_replay_uses_default_sqlite_path() {
        let workspace = tempfile::tempdir().unwrap();

        let ledger = replay_ledger(None, None, workspace.path()).unwrap();

        assert_eq!(
            ledger,
            ReplayLedger::Sqlite(default_sqlite_path(workspace.path()).unwrap())
        );
    }

    #[test]
    fn replay_file_stays_explicit_jsonl() {
        let workspace = tempfile::tempdir().unwrap();

        let ledger =
            replay_ledger(None, Some(PathBuf::from("events.jsonl")), workspace.path()).unwrap();

        assert_eq!(ledger, ReplayLedger::Jsonl(PathBuf::from("events.jsonl")));
    }

    #[test]
    fn default_sqlite_replay_fails_closed_when_daemon_lock_exists() {
        let workspace = tempfile::tempdir().unwrap();
        let socket = workspace.path().join("agent.sock");
        let _lock = plato_agent::daemon::lock::WorkspaceLock::acquire_for_workspace(
            workspace.path(),
            &socket,
        )
        .unwrap();
        let ledger = ReplayLedger::Sqlite(default_sqlite_path(workspace.path()).unwrap());
        let mut stdout = Vec::new();

        let error = write_replay_output(&mut stdout, ledger, None, workspace.path()).unwrap_err();

        assert!(matches!(error, AppError::DaemonLockHeld { .. }));
        assert!(stdout.is_empty());
    }
}
