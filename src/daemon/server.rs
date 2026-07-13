use crate::{
    AppResult,
    daemon::{handlers::handle_line, lock::WorkspaceLock, runtime::DaemonRuntime},
    paths,
};
use std::{
    fs::{self, DirBuilder, Permissions},
    io::{BufRead, BufReader, Error, ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonPaths {
    pub workspace_root: PathBuf,
    pub workspace_id: String,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub ledger_path: PathBuf,
}

impl DaemonPaths {
    pub fn resolve(workspace_root: &Path, socket_path: Option<PathBuf>) -> AppResult<Self> {
        let workspace_root = workspace_root.canonicalize()?;
        let workspace_id = paths::workspace_id(&workspace_root)?;
        let socket_path = socket_path.unwrap_or(paths::default_socket_path(&workspace_root)?);
        Ok(Self {
            lock_path: paths::default_lock_path(&workspace_root)?,
            ledger_path: paths::default_sqlite_path(&workspace_root)?,
            workspace_root,
            workspace_id,
            socket_path,
        })
    }
}

#[derive(Debug)]
pub struct DaemonServer {
    listener: UnixListener,
    runtime: DaemonRuntime,
    _lock: WorkspaceLock,
}

impl DaemonServer {
    pub fn bind(workspace_root: &Path, socket_path: Option<PathBuf>) -> AppResult<Self> {
        #[cfg(test)]
        {
            paths::with_test_xdg(workspace_root, || {
                Self::bind_inner(workspace_root, socket_path)
            })
        }
        #[cfg(not(test))]
        Self::bind_inner(workspace_root, socket_path)
    }

    fn bind_inner(workspace_root: &Path, socket_path: Option<PathBuf>) -> AppResult<Self> {
        let runtime_home = paths::runtime_home()?;
        let paths = DaemonPaths::resolve(workspace_root, socket_path)?;
        prepare_runtime_path(&runtime_home, &paths.lock_path)?;
        prepare_socket_parent(&runtime_home, &paths.socket_path)?;
        let lock = WorkspaceLock::acquire_for_workspace(&paths.workspace_root, &paths.socket_path)?;
        crate::ledger::interrupt_orphaned_sqlite_runs(&paths.ledger_path)?;
        if paths.socket_path.exists() {
            fs::remove_file(&paths.socket_path)?;
        }
        let listener = UnixListener::bind(&paths.socket_path)?;
        if let Err(error) = restrict_socket(&paths.socket_path) {
            drop(listener);
            let _ = fs::remove_file(&paths.socket_path);
            return Err(error.into());
        }
        let runtime = DaemonRuntime::new(paths);
        Ok(Self {
            listener,
            runtime,
            _lock: lock,
        })
    }

    pub fn paths(&self) -> &DaemonPaths {
        &self.runtime.paths
    }

    pub fn serve_forever(&self, shutdown: Arc<AtomicBool>) -> AppResult<()> {
        for stream in self.listener.incoming() {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let stream = stream?;
            let runtime = self.runtime.clone();
            thread::spawn(move || {
                if let Err(error) = handle_stream(runtime, stream) {
                    eprintln!("daemon connection error: {error}");
                }
            });
        }
        Ok(())
    }

    pub fn serve_next(&self) -> AppResult<()> {
        let (stream, _) = self.listener.accept()?;
        handle_stream(self.runtime.clone(), stream)
    }

    #[cfg(test)]
    fn handle_line(&self, line: &str) -> crate::daemon::protocol::Envelope {
        handle_line(&self.runtime, line)
    }
}

fn prepare_runtime_path(runtime_home: &Path, path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "runtime path has no parent"))?;
    prepare_private_directory(parent, Some(runtime_home))
}

fn prepare_socket_parent(runtime_home: &Path, socket_path: &Path) -> std::io::Result<()> {
    let parent = socket_path
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "socket path has no parent"))?;
    let root = parent.starts_with(runtime_home).then_some(runtime_home);
    prepare_private_directory(parent, root)
}

fn prepare_private_directory(parent: &Path, root: Option<&Path>) -> std::io::Result<()> {
    if root.is_some_and(|root| !parent.starts_with(root)) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "private directory is outside its runtime root",
        ));
    }
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIRECTORY_MODE)
        .create(parent)?;

    if let Some(root) = root {
        for directory in parent
            .ancestors()
            .take_while(|directory| directory.starts_with(root))
        {
            restrict_private_directory(directory)?;
        }
    } else {
        restrict_private_directory(parent)?;
    }
    Ok(())
}

fn restrict_private_directory(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "private runtime path is not a directory: {}",
                path.display()
            ),
        ));
    }
    fs::set_permissions(path, Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
    verify_mode(path, PRIVATE_DIRECTORY_MODE)
}

fn restrict_socket(path: &Path) -> std::io::Result<()> {
    fs::set_permissions(path, Permissions::from_mode(SOCKET_MODE))?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!("daemon socket path is not a socket: {}", path.display()),
        ));
    }
    verify_mode(path, SOCKET_MODE)
}

fn verify_mode(path: &Path, expected: u32) -> std::io::Result<()> {
    let actual = fs::symlink_metadata(path)?.permissions().mode() & 0o777;
    if actual == expected {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::PermissionDenied,
        format!(
            "unsafe permissions on {}: expected {expected:04o}, got {actual:04o}",
            path.display()
        ),
    ))
}

fn handle_stream(runtime: DaemonRuntime, stream: UnixStream) -> AppResult<()> {
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = handle_line(&runtime, &line);
        serde_json::to_writer(&mut writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.runtime.paths.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppError,
        daemon::{
            client::DaemonClient,
            protocol::{
                ERROR_INTERNAL, ERROR_LAGGED, ERROR_MALFORMED_REQUEST, ERROR_NOT_FOUND,
                ERROR_OVERLOAD, ERROR_RUN_FAILED, ERROR_SESSIONS_LIST_FAILED,
                ERROR_WORKSPACE_MISMATCH, Envelope, EnvelopeKind, ProtocolError, RunStateName,
            },
            runtime::{MAX_EVENT_BUFFER, PendingApproval, RunRecord},
        },
        ledger::SqliteLedger,
        tools::ApprovalOutcome,
    };
    use platonic_core::{
        AgentId, ContextPack, HarnessEvent, Message, MessageRole, ModelName, ModelUsage,
        RecordedEvent, RunId, TurnId,
    };
    use serde_json::json;
    use std::{
        io::{BufRead, Read},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        sync::{Arc, Barrier, mpsc},
        thread,
        time::{Duration, Instant},
    };

    const FAKE_PROVIDER_TIMEOUT: Duration = Duration::from_secs(15);

    #[test]
    fn bind_sets_private_socket_permissions() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_root = tempfile::tempdir().unwrap();
        let parent = socket_root.path().join("private").join("nested");
        let socket_path = parent.join("agent.sock");

        let server = DaemonServer::bind(workspace.path(), Some(socket_path.clone())).unwrap();

        assert_eq!(mode(&parent), PRIVATE_DIRECTORY_MODE);
        assert_eq!(mode(&socket_path), SOCKET_MODE);
        assert!(
            server
                .paths()
                .ledger_path
                .starts_with(workspace.path().join("xdg-state"))
        );
        assert!(
            server
                .paths()
                .lock_path
                .starts_with(workspace.path().join("xdg-runtime"))
        );
        drop(server);
    }

    #[test]
    fn bind_restricts_preexisting_wide_custom_socket_parent() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_root = tempfile::tempdir().unwrap();
        let parent = socket_root.path().join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, Permissions::from_mode(0o755)).unwrap();
        let socket_path = parent.join("agent.sock");

        let server = DaemonServer::bind(workspace.path(), Some(socket_path.clone())).unwrap();

        assert_eq!(mode(&parent), PRIVATE_DIRECTORY_MODE);
        assert_eq!(mode(&socket_path), SOCKET_MODE);
        drop(server);
    }

    #[test]
    fn mode_verification_rejects_wide_permissions() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), Permissions::from_mode(0o755)).unwrap();

        let error = verify_mode(parent.path(), PRIVATE_DIRECTORY_MODE).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("expected 0700, got 0755"));
    }

    #[test]
    fn runtime_permission_hardening_covers_the_private_chain() {
        let root_parent = tempfile::tempdir().unwrap();
        let root = root_parent.path().join("user");
        let middle = root.join("plato-agent");
        let leaf = middle.join("workspaces").join("workspace-1");
        fs::create_dir_all(&leaf).unwrap();
        for path in [&root, &middle, &leaf] {
            fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
        }

        prepare_private_directory(&leaf, Some(&root)).unwrap();

        assert_eq!(mode(&root), PRIVATE_DIRECTORY_MODE);
        assert_eq!(mode(&middle), PRIVATE_DIRECTORY_MODE);
        assert_eq!(mode(&middle.join("workspaces")), PRIVATE_DIRECTORY_MODE);
        assert_eq!(mode(&leaf), PRIVATE_DIRECTORY_MODE);
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn hello_round_trip_over_unix_socket() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path.clone())).unwrap();
        let paths = server.paths().clone();

        let handle = thread::spawn(move || server.serve_next().unwrap());

        let mut stream = UnixStream::connect(&socket_path).unwrap();
        writeln!(
            stream,
            r#"{{"v":1,"id":"req_1","kind":"request","method":"hello","params":{{"workspace_root":"{}","workspace_id":"{}"}}}}"#,
            paths.workspace_root.display(),
            paths.workspace_id
        )
        .unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();

        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        handle.join().unwrap();
        let response: Envelope = serde_json::from_str(raw.trim()).unwrap();

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(response.id.as_deref(), Some("req_1"));
        assert_eq!(response.method.as_deref(), Some("hello"));
        let result = response.result.unwrap();
        assert_eq!(result["workspace_id"], paths.workspace_id);
        assert_eq!(
            result["capabilities"],
            serde_json::json!([
                "hello",
                "run.start",
                "message.append",
                "events.stream",
                "approval.decide",
                "run.cancel",
                "sessions.list",
                "transcript.read",
                "transcript.read.typed"
            ])
        );
    }

    #[test]
    fn hello_rejects_workspace_mismatch() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path.clone())).unwrap();

        let handle = thread::spawn(move || server.serve_next().unwrap());

        let mut stream = UnixStream::connect(&socket_path).unwrap();
        writeln!(
            stream,
            r#"{{"v":1,"id":"req_1","kind":"request","method":"hello","params":{{"workspace_root":"{}","workspace_id":"wrong"}}}}"#,
            workspace.path().display()
        )
        .unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();

        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        handle.join().unwrap();
        let response: Envelope = serde_json::from_str(raw.trim()).unwrap();
        let error: ProtocolError = response.error.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Error);
        assert_eq!(error.code, ERROR_WORKSPACE_MISMATCH);
    }

    #[test]
    fn run_start_reports_shared_driver_error() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let config_path = workspace.path().join("plato.toml");
        std::fs::write(
            &config_path,
            r#"
[provider]
kind = "open_ai"
model = "gpt-5.5"
api_key_env = "PLATO_AGENT_TEST_MISSING_KEY"
"#,
        )
        .unwrap();
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();

        let response = server.handle_line(&format!(
            r#"{{"v":1,"id":"run_1","kind":"request","method":"run.start","params":{{"question":"hello","config_path":"{}","wait":true}}}}"#,
            config_path.display()
        ));
        let error = response.error.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Error);
        assert_eq!(response.method.as_deref(), Some("run.start"));
        assert_eq!(error.code, ERROR_RUN_FAILED);
        assert!(error.message.contains("PLATO_AGENT_TEST_MISSING_KEY"));
    }

    #[test]
    fn run_start_without_wait_returns_while_approval_is_pending_on_same_connection() {
        let provider = spawn_tool_call_provider();
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let config_path = workspace.path().join("plato.toml");
        write_provider_config(&config_path, &provider.base_url, "file.write");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path.clone())).unwrap();
        let handle = thread::spawn(move || server.serve_next().unwrap());
        let mut stream = UnixStream::connect(&socket_path).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());

        writeln!(
            stream,
            r#"{{"v":1,"id":"run_1","kind":"request","method":"run.start","params":{{"question":"write a file","config_path":"{}"}}}}"#,
            config_path.display()
        )
        .unwrap();
        let response = read_envelope(&mut reader);
        assert_eq!(response.kind, EnvelopeKind::Response);
        let result = response.result.unwrap();
        assert_eq!(result["status"], "running");
        assert!(result["final_answer"].is_null());
        let run_id = result["run_id"].as_str().unwrap().to_string();

        let mut approval_seen = false;
        let mut last_events = serde_json::Value::Null;
        for attempt in 0..100 {
            writeln!(
                stream,
                r#"{{"v":1,"id":"events_{attempt}","kind":"request","method":"events.stream","params":{{"run_id":"{}","from_offset":0,"limit":32}}}}"#,
                run_id
            )
            .unwrap();
            let response = read_envelope(&mut reader);
            assert_eq!(response.kind, EnvelopeKind::Response);
            let events = response.result.unwrap()["events"].clone();
            last_events = events.clone();
            approval_seen = events_contain_approval_request(&events);
            if approval_seen {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            approval_seen,
            "single connection should keep serving lines; last events: {last_events}"
        );

        writeln!(
            stream,
            r#"{{"v":1,"id":"deny_1","kind":"request","method":"approval.decide","params":{{"run_id":"{}","tool_call_id":"call_1","decision":"deny","reason":"test done"}}}}"#,
            run_id
        )
        .unwrap();
        let response = read_envelope(&mut reader);
        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(response.result.unwrap()["status"], "running");

        stream.shutdown(std::net::Shutdown::Write).unwrap();
        handle.join().unwrap();
        let _provider_request = provider.handle.join().unwrap();
    }

    #[test]
    fn run_cancel_unblocks_pending_approval_within_bound() {
        let provider = spawn_tool_call_provider();
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let config_path = workspace.path().join("plato.toml");
        write_provider_config(&config_path, &provider.base_url, "file.write");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();

        let response = server.handle_line(&format!(
            r#"{{"v":1,"id":"run_1","kind":"request","method":"run.start","params":{{"question":"write a file","config_path":"{}"}}}}"#,
            config_path.display()
        ));
        let run_id = response.result.unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let record = server.runtime.runs.lock().unwrap()[&run_id].clone();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !record.approvals.lock().unwrap().contains_key("call_1") {
            assert!(Instant::now() < deadline, "approval did not become pending");
            thread::sleep(Duration::from_millis(5));
        }

        let response = server.handle_line(&format!(
            r#"{{"v":1,"id":"cancel_1","kind":"request","method":"run.cancel","params":{{"run_id":"{run_id}"}}}}"#
        ));
        assert_eq!(response.kind, EnvelopeKind::Response);
        let deadline = Instant::now() + Duration::from_secs(2);
        while record.status().state == RunStateName::Running {
            assert!(
                Instant::now() < deadline,
                "canceled approval worker did not exit"
            );
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(record.status().state, RunStateName::Canceled);
        provider.handle.join().unwrap();
    }

    #[test]
    fn different_sessions_run_concurrently_with_separate_ledgers() {
        let provider = spawn_concurrent_text_provider();
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let config_path = workspace.path().join("plato.toml");
        write_provider_config(&config_path, &provider.base_url, "file.read");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();

        let first = server.handle_line(&format!(
            r#"{{"v":1,"id":"run_1","kind":"request","method":"run.start","params":{{"question":"question one","config_path":"{}"}}}}"#,
            config_path.display()
        ));
        assert_eq!(first.kind, EnvelopeKind::Response, "{:?}", first.error);
        let first = first.result.unwrap();
        assert_eq!(first["status"], "running");

        let second = server.handle_line(&format!(
            r#"{{"v":1,"id":"run_2","kind":"request","method":"run.start","params":{{"question":"question two","config_path":"{}"}}}}"#,
            config_path.display()
        ));
        assert_eq!(second.kind, EnvelopeKind::Response, "{:?}", second.error);
        let second = second.result.unwrap();

        let first_run = first["run_id"].as_str().unwrap();
        let first_session = first["session_id"].as_str().unwrap();
        let second_run = second["run_id"].as_str().unwrap();
        let second_session = second["session_id"].as_str().unwrap();
        assert_ne!(first_run, second_run);
        assert_ne!(first_session, second_session);

        wait_for_finished_run(&server, first_run);
        wait_for_finished_run(&server, second_run);
        let requests = provider.handle.join().unwrap();
        assert_eq!(requests.len(), 2);

        let ledger = SqliteLedger::open_readonly(&server.paths().ledger_path).unwrap();
        for (session_id, run_id, question, answer) in [
            (first_session, first_run, "question one", "answer one"),
            (second_session, second_run, "question two", "answer two"),
        ] {
            let session = ledger.read_session(session_id).unwrap();
            assert_eq!(session.runs.len(), 1);
            assert_eq!(session.runs[0].run_id, run_id);
            assert!(
                session.runs[0]
                    .records
                    .iter()
                    .all(|record| record.event.run_id().to_string() == run_id)
            );
            assert!(matches!(
                session.runs[0].records.last().map(|record| &record.event),
                Some(HarnessEvent::RunFinished { .. })
            ));

            let turns = ledger.session_turns(session_id).unwrap();
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].question, question);
            assert_eq!(turns[0].final_answer, answer);
        }
    }

    #[test]
    fn concurrent_message_append_reserves_only_one_session_run() {
        const CLIENTS: usize = 64;

        let provider = spawn_tool_call_provider();
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let config_path = workspace.path().join("plato.toml");
        write_provider_config(&config_path, &provider.base_url, "file.write");
        let server = Arc::new(DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap());
        seed_finished_session(
            &server.paths().ledger_path,
            "seed_run",
            "shared_session",
            "seed answer",
        );
        let barrier = Arc::new(Barrier::new(CLIENTS + 1));
        let mut clients = Vec::new();
        for index in 0..CLIENTS {
            let server = Arc::clone(&server);
            let barrier = Arc::clone(&barrier);
            let config_path = config_path.clone();
            clients.push(thread::spawn(move || {
                barrier.wait();
                server.handle_line(&format!(
                    r#"{{"v":1,"id":"append_{index}","kind":"request","method":"message.append","params":{{"message":"write a file","session_id":"shared_session","config_path":"{}"}}}}"#,
                    config_path.display()
                ))
            }));
        }
        barrier.wait();
        let responses = clients
            .into_iter()
            .map(|client| client.join().unwrap())
            .collect::<Vec<_>>();
        let admitted_run_ids = responses
            .iter()
            .filter(|response| response.kind == EnvelopeKind::Response)
            .filter_map(|response| response.result.as_ref())
            .filter_map(|result| result["run_id"].as_str())
            .map(str::to_string)
            .collect::<Vec<_>>();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let pending = admitted_run_ids.iter().any(|run_id| {
                server.runtime.runs.lock().unwrap()[run_id]
                    .approvals
                    .lock()
                    .unwrap()
                    .contains_key("call_1")
            });
            if pending {
                break;
            }
            assert!(Instant::now() < deadline, "approval did not become pending");
            thread::sleep(Duration::from_millis(5));
        }
        for run_id in &admitted_run_ids {
            server.handle_line(&format!(
                r#"{{"v":1,"id":"cancel","kind":"request","method":"run.cancel","params":{{"run_id":"{run_id}"}}}}"#
            ));
        }
        provider.handle.join().unwrap();

        assert_eq!(
            admitted_run_ids.len(),
            1,
            "one session admitted multiple concurrent runs: {admitted_run_ids:?}"
        );
        assert_eq!(
            responses
                .iter()
                .filter(|response| response.kind == EnvelopeKind::Error)
                .filter(|response| {
                    response.error.as_ref().map(|error| error.code.as_str()) == Some(ERROR_OVERLOAD)
                })
                .count(),
            CLIENTS - 1
        );
    }

    #[test]
    fn run_start_rejects_invalid_params_before_driver() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();

        let response = server.handle_line(
            r#"{"v":1,"id":"run_1","kind":"request","method":"run.start","params":{}}"#,
        );
        let error = response.error.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Error);
        assert_eq!(error.code, ERROR_MALFORMED_REQUEST);
    }

    #[test]
    fn events_stream_returns_buffered_events() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let record = Arc::new(RunRecord::new(
            "run_1".into(),
            "session_1".into(),
            server.paths().ledger_path.clone(),
        ));
        record.push_event(json!({"kind": "test"}));
        server
            .runtime
            .runs
            .lock()
            .unwrap()
            .insert("run_1".into(), record);

        let response = server.handle_line(
            r#"{"v":1,"id":"events_1","kind":"request","method":"events.stream","params":{"run_id":"run_1","from_offset":0,"limit":1}}"#,
        );
        let result = response.result.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(result["run_id"], "run_1");
        assert_eq!(result["events"].as_array().unwrap().len(), 1);
        assert_eq!(result["next_offset"], 1);
    }

    #[test]
    fn events_stream_next_offset_advances_by_returned_page() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let record = Arc::new(RunRecord::new(
            "run_1".into(),
            "session_1".into(),
            server.paths().ledger_path.clone(),
        ));
        record.push_event(json!({"kind": "first"}));
        record.push_event(json!({"kind": "second"}));
        server
            .runtime
            .runs
            .lock()
            .unwrap()
            .insert("run_1".into(), record);

        let first = server.handle_line(
            r#"{"v":1,"id":"events_1","kind":"request","method":"events.stream","params":{"run_id":"run_1","from_offset":0,"limit":1}}"#,
        );
        let second = server.handle_line(
            r#"{"v":1,"id":"events_2","kind":"request","method":"events.stream","params":{"run_id":"run_1","from_offset":1,"limit":1}}"#,
        );

        let first = first.result.unwrap();
        let second = second.result.unwrap();
        assert_eq!(first["next_offset"], 1);
        assert_eq!(first["events"][0]["event"]["kind"], "first");
        assert_eq!(second["next_offset"], 2);
        assert_eq!(second["events"][0]["event"]["kind"], "second");
    }

    #[test]
    fn events_stream_reports_lagged_offsets() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let record = Arc::new(RunRecord::new(
            "run_1".into(),
            "session_1".into(),
            server.paths().ledger_path.clone(),
        ));
        for index in 0..(MAX_EVENT_BUFFER + 1) {
            record.push_event(json!({"index": index}));
        }
        server
            .runtime
            .runs
            .lock()
            .unwrap()
            .insert("run_1".into(), record);

        let response = server.handle_line(
            r#"{"v":1,"id":"events_1","kind":"request","method":"events.stream","params":{"run_id":"run_1","from_offset":0}}"#,
        );
        let error = response.error.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Error);
        assert_eq!(error.code, ERROR_LAGGED);
    }

    #[test]
    fn client_recovers_from_lag_at_tip_with_typed_final_state() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path.clone())).unwrap();
        seed_finished_session(&server.paths().ledger_path, "run_1", "session_1", "done");
        let record = Arc::new(RunRecord::new(
            "run_1".into(),
            "session_1".into(),
            server.paths().ledger_path.clone(),
        ));
        record.set_finished("done".into());
        for index in 0..(MAX_EVENT_BUFFER + 1) {
            record.push_event(json!({"index": index}));
        }
        server
            .runtime
            .runs
            .lock()
            .unwrap()
            .insert("run_1".into(), record);
        let handle = thread::spawn(move || server.serve_next().unwrap());
        let mut client = DaemonClient::connect(&socket_path).unwrap();

        let error = client.events_stream("run_1", Some(0), 16).unwrap_err();
        assert!(matches!(
            error,
            AppError::DaemonResponse(ProtocolError { code, .. }) if code == ERROR_LAGGED
        ));

        let tail = client.events_stream("run_1", None, 16).unwrap();
        assert_eq!(tail.from_offset, (MAX_EVENT_BUFFER + 1) as u64);
        assert_eq!(tail.next_offset, tail.from_offset);
        assert!(tail.events.is_empty());
        assert_eq!(tail.status, RunStateName::Finished);

        let transcript = client.transcript_read("run_1").unwrap();
        assert_eq!(transcript.status, RunStateName::Finished);
        assert_eq!(transcript.final_answer.as_deref(), Some("done"));

        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn client_recovers_typed_final_answer_after_daemon_restart() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let first_socket = socket_dir.path().join("agent-1.sock");
        let first_server = DaemonServer::bind(workspace.path(), Some(first_socket)).unwrap();
        seed_finished_session(
            &first_server.paths().ledger_path,
            "run_1",
            "session_1",
            "persisted answer",
        );
        drop(first_server);

        let second_socket = socket_dir.path().join("agent-2.sock");
        let second_server =
            DaemonServer::bind(workspace.path(), Some(second_socket.clone())).unwrap();
        let handle = thread::spawn(move || second_server.serve_next().unwrap());
        let mut client = DaemonClient::connect(&second_socket).unwrap();

        let sessions = client.sessions_list().unwrap();
        assert_eq!(sessions[0].session_id, "session_1");
        assert_eq!(sessions[0].status, RunStateName::Finished);

        let transcript = client.transcript_read_session("session_1").unwrap();
        assert_eq!(transcript.run_id, "run_1");
        assert_eq!(transcript.status, RunStateName::Finished);
        assert_eq!(transcript.final_answer.as_deref(), Some("persisted answer"));

        drop(client);
        handle.join().unwrap();
    }

    #[test]
    fn transcript_read_returns_one_typed_run_or_all_session_runs_in_order() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let server =
            DaemonServer::bind(workspace.path(), Some(socket_dir.path().join("agent.sock")))
                .unwrap();
        seed_finished_session_run(
            &server.paths().ledger_path,
            "run_1",
            "session_1",
            "first question",
            "first answer",
            true,
        );
        seed_finished_session_run(
            &server.paths().ledger_path,
            "run_2",
            "session_1",
            "second question",
            "second answer",
            false,
        );

        let run = server.handle_line(
            r#"{"v":1,"id":"run_read","kind":"request","method":"transcript.read","params":{"run_id":"run_2"}}"#,
        );
        assert_eq!(run.kind, EnvelopeKind::Response);
        assert_eq!(
            run.result.unwrap(),
            json!({
                "run_id": "run_2",
                "status": "finished",
                "final_answer": "second answer",
                "transcript": "final_phase: Finished\nnext_seq: 5\n[turn_run_2] assistant: second answer",
                "typed": {
                    "runs": [{
                        "run_id": "run_2",
                        "session_index": 1,
                        "status": "finished",
                        "entries": [
                            {"kind": "user", "text": "second question"},
                            {"kind": "assistant", "text": "second answer"}
                        ]
                    }]
                }
            })
        );

        let session = server.handle_line(
            r#"{"v":1,"id":"session_read","kind":"request","method":"transcript.read","params":{"session_id":"session_1"}}"#,
        );
        assert_eq!(session.kind, EnvelopeKind::Response);
        assert_eq!(
            session.result.unwrap(),
            json!({
                "run_id": "run_2",
                "status": "finished",
                "final_answer": "second answer",
                "transcript": concat!(
                    "session_id: session_1\n",
                    "run_id: run_1\n",
                    "final_phase: Finished\n",
                    "next_seq: 5\n",
                    "[turn_run_1] assistant: first answer\n",
                    "run_id: run_2\n",
                    "final_phase: Finished\n",
                    "next_seq: 5\n",
                    "[turn_run_2] assistant: second answer"
                ),
                "typed": {
                    "runs": [
                        {
                            "run_id": "run_1",
                            "session_index": 0,
                            "status": "finished",
                            "entries": [
                                {"kind": "user", "text": "first question"},
                                {"kind": "assistant", "text": "first answer"}
                            ]
                        },
                        {
                            "run_id": "run_2",
                            "session_index": 1,
                            "status": "finished",
                            "entries": [
                                {"kind": "user", "text": "second question"},
                                {"kind": "assistant", "text": "second answer"}
                            ]
                        }
                    ]
                }
            })
        );
    }

    #[test]
    fn transcript_read_distinguishes_missing_from_internal_failures() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let mut server =
            DaemonServer::bind(workspace.path(), Some(socket_dir.path().join("agent.sock")))
                .unwrap();
        server.runtime.paths.ledger_path = workspace.path().join("agent.db");

        let missing = server.handle_line(
            r#"{"v":1,"id":"transcript_1","kind":"request","method":"transcript.read","params":{"run_id":"run_missing"}}"#,
        );
        assert_eq!(missing.kind, EnvelopeKind::Error);
        assert_eq!(missing.error.unwrap().code, ERROR_NOT_FOUND);

        std::fs::write(&server.paths().ledger_path, b"not a sqlite database").unwrap();
        let corrupt = server.handle_line(
            r#"{"v":1,"id":"transcript_2","kind":"request","method":"transcript.read","params":{"run_id":"run_missing"}}"#,
        );
        assert_eq!(corrupt.kind, EnvelopeKind::Error);
        assert_eq!(corrupt.error.unwrap().code, ERROR_INTERNAL);
    }

    #[test]
    fn approval_decide_updates_pending_request() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let record = Arc::new(RunRecord::new(
            "run_1".into(),
            "session_1".into(),
            server.paths().ledger_path.clone(),
        ));
        record
            .approvals
            .lock()
            .unwrap()
            .insert("call_1".into(), PendingApproval::new());
        server
            .runtime
            .runs
            .lock()
            .unwrap()
            .insert("run_1".into(), record.clone());

        let response = server.handle_line(
            r#"{"v":1,"id":"approval_1","kind":"request","method":"approval.decide","params":{"run_id":"run_1","tool_call_id":"call_1","decision":"grant"}}"#,
        );

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(
            record.approvals.lock().unwrap()["call_1"].decision,
            Some(ApprovalOutcome::Granted)
        );
    }

    #[test]
    fn run_cancel_synchronizes_with_pending_approval() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = Arc::new(DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap());
        let record = Arc::new(RunRecord::new(
            "run_1".into(),
            "session_1".into(),
            server.paths().ledger_path.clone(),
        ));
        server
            .runtime
            .runs
            .lock()
            .unwrap()
            .insert("run_1".into(), record.clone());
        let mut approvals = record.approvals.lock().unwrap();
        approvals.insert("call_1".into(), PendingApproval::new());
        let (sender, receiver) = mpsc::channel();
        let (started_sender, started_receiver) = mpsc::channel();
        let cancel_server = Arc::clone(&server);
        let cancel = thread::spawn(move || {
            started_sender.send(()).unwrap();
            sender
                .send(cancel_server.handle_line(
                    r#"{"v":1,"id":"cancel_1","kind":"request","method":"run.cancel","params":{"run_id":"run_1"}}"#,
                ))
                .unwrap();
        });

        started_receiver.recv().unwrap();
        assert!(
            receiver.recv_timeout(Duration::from_millis(100)).is_err(),
            "run.cancel must synchronize with the approval waiter mutex"
        );
        drop(approvals);
        let response = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        cancel.join().unwrap();

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert!(record.cancel.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn sessions_list_reports_run_projection() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let record = Arc::new(RunRecord::new(
            "run_1".into(),
            "session_1".into(),
            server.paths().ledger_path.clone(),
        ));
        server
            .runtime
            .runs
            .lock()
            .unwrap()
            .insert("run_1".into(), record);

        let response = server
            .handle_line(r#"{"v":1,"id":"sessions_1","kind":"request","method":"sessions.list"}"#);
        let result = response.result.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(result["sessions"][0]["session_id"], "session_1");
        assert_eq!(result["sessions"][0]["run_id"], "run_1");
        assert_eq!(result["sessions"][0]["status"], "running");
    }

    #[test]
    fn sessions_list_reports_persisted_sessions_after_restart() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let first_socket = socket_dir.path().join("agent-1.sock");
        let first_server = DaemonServer::bind(workspace.path(), Some(first_socket)).unwrap();
        let ledger_path = first_server.paths().ledger_path.clone();
        let mut ledger = SqliteLedger::open_or_create(&ledger_path).unwrap();
        let run_id = RunId::new("run_1").unwrap();
        ledger
            .begin_session_run("session_1", &run_id, "first question", true)
            .unwrap();
        ledger.finish_session_run(&run_id, "first answer").unwrap();
        drop(ledger);
        drop(first_server);

        let second_socket = socket_dir.path().join("agent-2.sock");
        let second_server = DaemonServer::bind(workspace.path(), Some(second_socket)).unwrap();
        let response = second_server
            .handle_line(r#"{"v":1,"id":"sessions_1","kind":"request","method":"sessions.list"}"#);
        let result = response.result.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(result["sessions"][0]["session_id"], "session_1");
        assert_eq!(result["sessions"][0]["run_id"], "run_1");
        assert_eq!(result["sessions"][0]["status"], "finished");
        assert_eq!(result["sessions"][0]["latest_question"], "first question");
    }

    #[test]
    fn sessions_list_empty_fresh_workspace_does_not_create_ledger() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let ledger_path = server.paths().ledger_path.clone();
        assert!(!ledger_path.exists());

        let response = server
            .handle_line(r#"{"v":1,"id":"sessions_1","kind":"request","method":"sessions.list"}"#);
        let result = response.result.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(result["sessions"].as_array().unwrap().len(), 0);
        assert!(!ledger_path.exists());
    }

    #[test]
    fn sessions_list_failure_uses_dedicated_error_code() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let ledger_path = &server.paths().ledger_path;
        std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
        std::fs::write(ledger_path, "not a sqlite database").unwrap();

        let response = server
            .handle_line(r#"{"v":1,"id":"sessions_1","kind":"request","method":"sessions.list"}"#);
        let error = response.error.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Error);
        assert_eq!(response.method.as_deref(), Some("sessions.list"));
        assert_eq!(error.code, ERROR_SESSIONS_LIST_FAILED);
    }

    #[test]
    fn sessions_list_marks_orphaned_running_session_interrupted_after_restart() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let first_socket = socket_dir.path().join("agent-1.sock");
        let first_server = DaemonServer::bind(workspace.path(), Some(first_socket)).unwrap();
        let ledger_path = first_server.paths().ledger_path.clone();
        let mut ledger = SqliteLedger::open_or_create(&ledger_path).unwrap();
        let run_id = RunId::new("run_1").unwrap();
        ledger
            .begin_session_run("session_1", &run_id, "first question", true)
            .unwrap();
        drop(ledger);
        drop(first_server);

        let second_socket = socket_dir.path().join("agent-2.sock");
        let second_server = DaemonServer::bind(workspace.path(), Some(second_socket)).unwrap();
        let response = second_server
            .handle_line(r#"{"v":1,"id":"sessions_1","kind":"request","method":"sessions.list"}"#);
        let result = response.result.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(result["sessions"][0]["session_id"], "session_1");
        assert_eq!(result["sessions"][0]["run_id"], "run_1");
        assert_eq!(result["sessions"][0]["status"], "interrupted");
    }

    #[test]
    fn daemon_startup_reconciles_orphaned_running_session_for_resume() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let first_socket = socket_dir.path().join("agent-1.sock");
        let first_server = DaemonServer::bind(workspace.path(), Some(first_socket)).unwrap();
        let ledger_path = first_server.paths().ledger_path.clone();
        let mut ledger = SqliteLedger::open_or_create(&ledger_path).unwrap();
        ledger
            .begin_session_run(
                "session_1",
                &RunId::new("run_1").unwrap(),
                "first question",
                true,
            )
            .unwrap();
        drop(ledger);
        drop(first_server);

        let second_socket = socket_dir.path().join("agent-2.sock");
        let _second_server = DaemonServer::bind(workspace.path(), Some(second_socket)).unwrap();
        let mut ledger = SqliteLedger::open_or_create(&ledger_path).unwrap();

        assert_eq!(
            ledger.session_summaries().unwrap()[0].status,
            RunStateName::Interrupted
        );
        ledger
            .begin_session_run(
                "session_1",
                &RunId::new("run_2").unwrap(),
                "follow up",
                false,
            )
            .unwrap();
    }

    #[test]
    fn sessions_list_reports_latest_question_preview() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let mut ledger = SqliteLedger::open_or_create(&server.paths().ledger_path).unwrap();
        let run_id = RunId::new("run_1").unwrap();
        let long_question = format!("{}\nsecond line", "x".repeat(130));
        ledger
            .begin_session_run("session_1", &run_id, &long_question, true)
            .unwrap();
        ledger.finish_session_run(&run_id, "first answer").unwrap();
        drop(ledger);

        let response = server
            .handle_line(r#"{"v":1,"id":"sessions_1","kind":"request","method":"sessions.list"}"#);
        let result = response.result.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Response);
        assert_eq!(
            result["sessions"][0]["latest_question"],
            format!("{}...", "x".repeat(120))
        );
    }

    #[test]
    fn message_append_rejects_active_session_run() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let record = Arc::new(RunRecord::new(
            "run_1".into(),
            "session_1".into(),
            server.paths().ledger_path.clone(),
        ));
        server
            .runtime
            .runs
            .lock()
            .unwrap()
            .insert("run_1".into(), record);

        let response = server.handle_line(
            r#"{"v":1,"id":"append_1","kind":"request","method":"message.append","params":{"session_id":"session_1","message":"again"}}"#,
        );
        let error = response.error.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Error);
        assert_eq!(error.code, ERROR_OVERLOAD);
        assert!(error.message.contains("session already has an active run"));
    }

    #[test]
    fn message_append_without_wait_returns_running_by_default() {
        let provider = spawn_tool_call_provider();
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let config_path = workspace.path().join("plato.toml");
        write_provider_config(&config_path, &provider.base_url, "file.write");
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let mut ledger = SqliteLedger::open_or_create(&server.paths().ledger_path).unwrap();
        let prior_run = RunId::new("run_prior").unwrap();
        ledger
            .begin_session_run("session_1", &prior_run, "first question", true)
            .unwrap();
        ledger
            .finish_session_run(&prior_run, "first answer")
            .unwrap();
        drop(ledger);

        let response = server.handle_line(&format!(
            r#"{{"v":1,"id":"append_1","kind":"request","method":"message.append","params":{{"session_id":"session_1","message":"follow up","config_path":"{}"}}}}"#,
            config_path.display()
        ));
        assert_eq!(response.kind, EnvelopeKind::Response);
        let result = response.result.unwrap();
        assert_eq!(result["status"], "running");
        let run_id = result["run_id"].as_str().unwrap().to_string();

        let mut approval_seen = false;
        for attempt in 0..100 {
            let response = server.handle_line(&format!(
                r#"{{"v":1,"id":"events_{attempt}","kind":"request","method":"events.stream","params":{{"run_id":"{}","from_offset":0,"limit":32}}}}"#,
                run_id
            ));
            assert_eq!(response.kind, EnvelopeKind::Response);
            let events = response.result.unwrap()["events"].clone();
            approval_seen = events_contain_approval_request(&events);
            if approval_seen {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(approval_seen);

        let response = server.handle_line(&format!(
            r#"{{"v":1,"id":"deny_1","kind":"request","method":"approval.decide","params":{{"run_id":"{}","tool_call_id":"call_1","decision":"deny","reason":"test done"}}}}"#,
            run_id
        ));
        assert_eq!(response.kind, EnvelopeKind::Response);
        let _provider_request = provider.handle.join().unwrap();
    }

    #[test]
    fn message_append_hydrates_persisted_session_turns() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let config_path = workspace.path().join("plato.toml");
        std::fs::write(
            &config_path,
            r#"
[provider]
api_key_env = "PATH"
base_url = "http://127.0.0.1:9"
timeout_ms = 1

[limits]
token_budget = 4000
max_output_tokens = 1

[tools]
enabled = ["file.read"]
"#,
        )
        .unwrap();
        let server = DaemonServer::bind(workspace.path(), Some(socket_path)).unwrap();
        let mut ledger = SqliteLedger::open_or_create(&server.paths().ledger_path).unwrap();
        let prior_run = RunId::new("run_prior").unwrap();
        ledger
            .begin_session_run("session_1", &prior_run, "first question", true)
            .unwrap();
        ledger
            .finish_session_run(&prior_run, "first answer")
            .unwrap();
        drop(ledger);

        let response = server.handle_line(&format!(
            r#"{{"v":1,"id":"append_1","kind":"request","method":"message.append","params":{{"session_id":"session_1","message":"follow up","config_path":"{}","wait":true}}}}"#,
            config_path.display()
        ));
        assert_eq!(response.kind, EnvelopeKind::Error);

        let ledger = SqliteLedger::open_readonly(&server.paths().ledger_path).unwrap();
        let (_run_id, records) = ledger.read_latest_run().unwrap();
        let recent_turns = records
            .iter()
            .find_map(|record| match &record.event {
                HarnessEvent::ContextBuilt { context, .. } => context
                    .fragments
                    .iter()
                    .find(|fragment| fragment.source == "model.messages")
                    .map(|fragment| fragment.content.as_str()),
                _ => None,
            })
            .expect("continued run should record model messages context");

        assert!(recent_turns.contains("first question"));
        assert!(recent_turns.contains("first answer"));
        assert!(recent_turns.contains("follow up"));
    }

    struct ToolCallProvider {
        base_url: String,
        handle: thread::JoinHandle<String>,
    }

    struct ConcurrentTextProvider {
        base_url: String,
        handle: thread::JoinHandle<Vec<String>>,
    }

    fn write_provider_config(path: &Path, base_url: &str, enabled_tool: &str) {
        let timeout_ms = FAKE_PROVIDER_TIMEOUT.as_millis();
        std::fs::write(
            path,
            format!(
                r#"
[provider]
kind = "open_ai"
model = "test-model"
api_key_env = "PATH"
base_url = "{base_url}"
timeout_ms = {timeout_ms}

[limits]
token_budget = 4000
max_output_tokens = 32
max_turns = 2

[tools]
enabled = ["{enabled_tool}"]
"#
            ),
        )
        .unwrap();
    }

    fn spawn_tool_call_provider() -> ToolCallProvider {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let body = concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"file_write\",\"arguments\":\"{\\\"path\\\":\\\"out.txt\\\",\\\"content\\\":\\\"hello\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        ToolCallProvider { base_url, handle }
    }

    fn spawn_concurrent_text_provider() -> ConcurrentTextProvider {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + FAKE_PROVIDER_TIMEOUT;
            let mut clients = Vec::new();
            while clients.len() < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let request = read_http_request(&mut stream);
                        clients.push((stream, request));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("provider accept failed: {error}"),
                }
            }
            assert_eq!(
                clients.len(),
                2,
                "both daemon runs must reach the provider before either response"
            );

            let mut requests = Vec::new();
            for (mut stream, request) in clients {
                let answer = if request.contains("question one") {
                    "answer one"
                } else if request.contains("question two") {
                    "answer two"
                } else {
                    panic!("provider received an unexpected request")
                };
                let content = json!({
                    "choices": [{
                        "index": 0,
                        "delta": {"content": answer},
                        "finish_reason": null
                    }]
                });
                let finish = json!({
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "stop"
                    }]
                });
                let body = format!("data: {content}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                requests.push(request);
            }
            requests
        });
        ConcurrentTextProvider { base_url, handle }
    }

    fn wait_for_finished_run(server: &DaemonServer, run_id: &str) {
        let deadline = Instant::now() + FAKE_PROVIDER_TIMEOUT;
        loop {
            let response = server.handle_line(&format!(
                r#"{{"v":1,"id":"events","kind":"request","method":"events.stream","params":{{"run_id":"{run_id}","from_offset":0,"limit":1}}}}"#
            ));
            assert_eq!(response.kind, EnvelopeKind::Response);
            let result = response.result.unwrap();
            match result["status"].as_str().unwrap() {
                "finished" => return,
                "running" => {}
                status => {
                    let record = server.runtime.runs.lock().unwrap()[run_id].clone();
                    panic!(
                        "run {run_id} ended as {status}: {:?}",
                        record.status().error
                    )
                }
            }
            assert!(Instant::now() < deadline, "run {run_id} did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn seed_finished_session(path: &Path, run_id: &str, session_id: &str, answer: &str) {
        seed_finished_session_run(path, run_id, session_id, "question", answer, true);
    }

    fn seed_finished_session_run(
        path: &Path,
        run_id: &str,
        session_id: &str,
        question: &str,
        answer: &str,
        create_session: bool,
    ) {
        let run_id = RunId::new(run_id).unwrap();
        let turn_id = TurnId::new(format!("turn_{}", run_id.as_str())).unwrap();
        let mut ledger = SqliteLedger::open_or_create(path).unwrap();
        ledger
            .begin_session_run(session_id, &run_id, question, create_session)
            .unwrap();
        let events = vec![
            HarnessEvent::RunStarted {
                run_id: run_id.clone(),
                agent_id: AgentId::new("agent_1").unwrap(),
            },
            HarnessEvent::ContextBuilt {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                context: ContextPack {
                    token_budget: 0,
                    fragments: vec![],
                },
            },
            HarnessEvent::ModelRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                step: 0,
                model: ModelName::new("model_1").unwrap(),
            },
            HarnessEvent::ModelResponded {
                run_id: run_id.clone(),
                turn_id,
                step: 0,
                output: Message {
                    role: MessageRole::Assistant,
                    content: answer.into(),
                },
                proposed_calls: vec![],
                usage: ModelUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
            },
            HarnessEvent::RunFinished {
                run_id: run_id.clone(),
            },
        ];
        for (seq, event) in events.into_iter().enumerate() {
            ledger
                .append(
                    run_id.as_str(),
                    &RecordedEvent {
                        seq: seq as u64,
                        occurred_at_ms: seq as u64,
                        event,
                    },
                )
                .unwrap();
        }
        ledger.finish_session_run(&run_id, answer).unwrap();
    }

    fn read_envelope(reader: &mut BufReader<UnixStream>) -> Envelope {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    fn events_contain_approval_request(events: &serde_json::Value) -> bool {
        events.as_array().unwrap().iter().any(|entry| {
            entry["event"]["kind"] == "approval_requested"
                && entry["event"]["tool_call_id"] == "call_1"
        })
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "client closed before headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = find_header_end(&bytes) {
                break header_end;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "client closed before body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn find_header_end(bytes: &[u8]) -> Option<usize> {
        bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
    }
}
