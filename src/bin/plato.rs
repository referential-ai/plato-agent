use clap::{Parser, Subcommand};
use plato_agent::{
    AppError, ApprovalMode, RunLedger, RunOptions, RunOutcome,
    daemon::lock::ensure_workspace_unlocked, paths::default_sqlite_path, replay_file,
    replay_sqlite, run_question,
};
use platonic_core::RunId;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(name = "plato")]
#[command(about = "One-shot Plato Agent CLI")]
struct Cli {
    #[arg(long, global = true, default_value = "plato.toml")]
    config: PathBuf,

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
    match cli.command {
        Some(Command::Replay { run, file }) => {
            let db_path = sqlite_path(cli.db, &workspace_root)?;
            match (db_path, file) {
                (Some(path), None) => {
                    ensure_workspace_unlocked(&workspace_root)?;
                    println!("{}", replay_sqlite(&path, run.as_deref())?);
                }
                (None, Some(file)) => {
                    if run.is_some() {
                        return Err(AppError::Config("replay --run requires --db".into()));
                    }
                    println!("{}", replay_file(&file)?);
                }
                (Some(_), Some(_)) => {
                    return Err(AppError::Config(
                        "replay accepts either --db or a JSONL file, not both".into(),
                    ));
                }
                (None, None) => {
                    return Err(AppError::Config(
                        "replay requires a JSONL file or --db".into(),
                    ));
                }
            }
            Ok(())
        }
        None => {
            let question = cli.question.join(" ");
            let ledger = run_ledger(cli.events, cli.db, &workspace_root)?;
            let outcome = run_question(RunOptions {
                question,
                config_path: cli.config,
                ledger: ledger.clone(),
                workspace_root,
                approval_mode: ApprovalMode::from_yolo(cli.yolo),
            })?;
            write_run_success_output(&mut io::stdout(), &mut io::stderr(), &outcome, &ledger)
        }
    }
}

fn run_ledger(
    events: Option<PathBuf>,
    db: Option<Option<PathBuf>>,
    workspace_root: &Path,
) -> plato_agent::AppResult<RunLedger> {
    let db_path = sqlite_path(db, workspace_root)?;
    if db_path.is_some() && events.is_some() {
        return Err(AppError::Config(
            "--events and --db are mutually exclusive".into(),
        ));
    }
    match db_path {
        Some(path) => {
            ensure_workspace_unlocked(workspace_root)?;
            Ok(RunLedger::Sqlite(path))
        }
        None => Ok(RunLedger::Jsonl(
            events.unwrap_or_else(|| PathBuf::from("events.jsonl")),
        )),
    }
}

fn sqlite_path(
    db: Option<Option<PathBuf>>,
    workspace_root: &Path,
) -> plato_agent::AppResult<Option<PathBuf>> {
    match db {
        None => Ok(None),
        Some(Some(path)) => Ok(Some(resolve_cli_path(path, workspace_root))),
        Some(None) => Ok(Some(default_sqlite_path(workspace_root)?)),
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
    fn explicit_sqlite_path_is_resolved_against_workspace_root() {
        let dir = tempfile::tempdir().unwrap();

        let path = sqlite_path(Some(Some(PathBuf::from("agent.db"))), dir.path()).unwrap();

        assert_eq!(path, Some(dir.path().join("agent.db")));
    }

    #[test]
    fn sqlite_run_fails_closed_when_daemon_lock_exists() {
        let workspace = tempfile::tempdir().unwrap();
        let socket = workspace.path().join("agent.sock");
        let _lock = plato_agent::daemon::lock::WorkspaceLock::acquire_for_workspace(
            workspace.path(),
            &socket,
        )
        .unwrap();

        let error = run_ledger(
            None,
            Some(Some(PathBuf::from("agent.db"))),
            workspace.path(),
        )
        .unwrap_err();

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
}
