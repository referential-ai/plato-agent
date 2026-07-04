use clap::{Parser, Subcommand};
use plato_agent::{
    AppError, ApprovalMode, RunLedger, RunOptions, paths::default_sqlite_path, replay_file,
    replay_sqlite, run_question,
};
use std::path::PathBuf;

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
            let db_path = sqlite_path(cli.db, &workspace_root)?;
            if db_path.is_some() && cli.events.is_some() {
                return Err(AppError::Config(
                    "--events and --db are mutually exclusive".into(),
                ));
            }
            let ledger = match db_path {
                Some(path) => RunLedger::Sqlite(path),
                None => {
                    RunLedger::Jsonl(cli.events.unwrap_or_else(|| PathBuf::from("events.jsonl")))
                }
            };
            run_question(RunOptions {
                question,
                config_path: cli.config,
                ledger,
                workspace_root,
                approval_mode: ApprovalMode::from_yolo(cli.yolo),
            })
        }
    }
}

fn sqlite_path(
    db: Option<Option<PathBuf>>,
    workspace_root: &std::path::Path,
) -> plato_agent::AppResult<Option<PathBuf>> {
    match db {
        None => Ok(None),
        Some(Some(path)) => Ok(Some(path)),
        Some(None) => Ok(Some(default_sqlite_path(workspace_root)?)),
    }
}
