use crate::{
    AppResult,
    daemon::{handlers::handle_line, lock::WorkspaceLock, runtime::DaemonRuntime},
    paths,
};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

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
        let paths = DaemonPaths::resolve(workspace_root, socket_path)?;
        if let Some(parent) = paths.socket_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = WorkspaceLock::acquire_for_workspace(&paths.workspace_root, &paths.socket_path)?;
        if paths.socket_path.exists() {
            fs::remove_file(&paths.socket_path)?;
        }
        let listener = UnixListener::bind(&paths.socket_path)?;
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
                let _ = handle_stream(runtime, stream);
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
        daemon::{
            protocol::{
                ERROR_LAGGED, ERROR_MALFORMED_REQUEST, ERROR_OVERLOAD, ERROR_RUN_FAILED,
                ERROR_WORKSPACE_MISMATCH, Envelope, EnvelopeKind, ProtocolError,
            },
            runtime::{MAX_EVENT_BUFFER, PendingApproval, RunRecord},
        },
        ledger::SqliteLedger,
        tools::ApprovalOutcome,
    };
    use platonic_core::{HarnessEvent, RunId};
    use serde_json::json;
    use std::{io::Read, sync::Arc, thread};

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
                "transcript.read"
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
            r#"{{"v":1,"id":"run_1","kind":"request","method":"run.start","params":{{"question":"hello","config_path":"{}"}}}}"#,
            config_path.display()
        ));
        let error = response.error.unwrap();

        assert_eq!(response.kind, EnvelopeKind::Error);
        assert_eq!(response.method.as_deref(), Some("run.start"));
        assert_eq!(error.code, ERROR_RUN_FAILED);
        assert!(error.message.contains("PLATO_AGENT_TEST_MISSING_KEY"));
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
}
