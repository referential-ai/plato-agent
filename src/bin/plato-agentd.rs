use clap::Parser;
use plato_agent::daemon::server::DaemonServer;
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

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
    let socket_path = server.paths().socket_path.clone();
    eprintln!("workspace_id: {}", server.paths().workspace_id);
    eprintln!("socket_path: {}", socket_path.display());
    eprintln!("ledger_path: {}", server.paths().ledger_path.display());

    let shutdown = Arc::new(AtomicBool::new(false));
    let mut signals = Signals::new([SIGINT, SIGTERM])?;

    {
        let shutdown = shutdown.clone();
        thread::spawn(move || {
            if signals.forever().next().is_some() {
                shutdown.store(true, Ordering::SeqCst);
                let _ = UnixStream::connect(&socket_path);
            }
        });
    }

    server.serve_forever(shutdown)
}
