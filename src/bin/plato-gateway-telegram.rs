use clap::Parser;
use plato_agent::telegram_gateway::{TelegramGatewayOptions, run_telegram_gateway};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "plato-gateway-telegram")]
#[command(about = "Telegram gateway for a local Plato Agent daemon")]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,

    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = run_telegram_gateway(TelegramGatewayOptions {
        workspace_root: cli.workspace,
        socket_path: cli.socket,
        config_path: cli.config,
    }) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
