use clap::{Parser, Subcommand};
use plato_agent::{ApprovalMode, RunOptions, replay_file, run_question};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "plato")]
#[command(about = "One-shot Plato Agent CLI")]
struct Cli {
    #[arg(long, global = true, default_value = "plato.toml")]
    config: PathBuf,

    #[arg(long, global = true, default_value = "events.jsonl")]
    events: PathBuf,

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
        #[arg(value_name = "FILE")]
        file: PathBuf,
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
    match cli.command {
        Some(Command::Replay { file }) => {
            println!("{}", replay_file(&file)?);
            Ok(())
        }
        None => {
            let question = cli.question.join(" ");
            run_question(RunOptions {
                question,
                config_path: cli.config,
                events_path: cli.events,
                workspace_root: std::env::current_dir()?,
                approval_mode: ApprovalMode::from_yolo(cli.yolo),
            })
        }
    }
}
