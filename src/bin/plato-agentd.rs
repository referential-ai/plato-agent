use clap::Parser;
use plato_agent::daemon::{server::DaemonServer, wake_listener};
#[cfg(unix)]
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
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
    install_shutdown_handler(shutdown.clone(), socket_path)?;

    server.serve_forever(shutdown)
}

#[cfg(unix)]
fn install_shutdown_handler(
    shutdown: Arc<AtomicBool>,
    socket_path: PathBuf,
) -> plato_agent::AppResult<()> {
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            request_shutdown(&shutdown, &socket_path);
        }
    });
    Ok(())
}

#[cfg(windows)]
fn install_shutdown_handler(
    shutdown: Arc<AtomicBool>,
    socket_path: PathBuf,
) -> plato_agent::AppResult<()> {
    ctrlc::set_handler(move || {
        request_shutdown(&shutdown, &socket_path);
    })
    .map_err(|error| {
        std::io::Error::other(format!(
            "failed to install console control handler: {error}"
        ))
    })?;
    Ok(())
}

fn request_shutdown(shutdown: &AtomicBool, socket_path: &std::path::Path) {
    shutdown.store(true, Ordering::SeqCst);
    wake_listener(socket_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_request_sets_flag_when_listener_is_missing() {
        let workspace = tempfile::tempdir().unwrap();
        let shutdown = AtomicBool::new(false);

        request_shutdown(&shutdown, &workspace.path().join("missing.sock"));

        assert!(shutdown.load(Ordering::SeqCst));
    }
}
