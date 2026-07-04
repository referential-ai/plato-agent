use clap::Parser;
use plato_agent::daemon::server::DaemonServer;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "plato-agentd")]
#[command(about = "Plato Agent local daemon")]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,

    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> plato_agent::AppResult<()> {
    let cli = Cli::parse();
    let server = DaemonServer::bind(&cli.workspace, cli.socket)?;
    eprintln!("workspace_id: {}", server.paths().workspace_id);
    eprintln!("socket_path: {}", server.paths().socket_path.display());
    eprintln!("ledger_path: {}", server.paths().ledger_path.display());
    server.serve_forever()
}
