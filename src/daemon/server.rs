use crate::{
    AppResult, ApprovalMode, RunLedger, RunOptions,
    daemon::{
        lock::WorkspaceLock,
        protocol::{
            ERROR_MALFORMED_REQUEST, ERROR_RUN_FAILED, ERROR_UNSUPPORTED_METHOD,
            ERROR_WORKSPACE_MISMATCH, Envelope, HelloParams, HelloResult, RunStartParams,
            RunStartResult, decode_request,
        },
    },
    paths, run_question,
};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
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
    paths: DaemonPaths,
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
        Ok(Self {
            listener,
            paths,
            _lock: lock,
        })
    }

    pub fn paths(&self) -> &DaemonPaths {
        &self.paths
    }

    pub fn serve_forever(&self) -> AppResult<()> {
        loop {
            self.serve_next()?;
        }
    }

    pub fn serve_next(&self) -> AppResult<()> {
        let (stream, _) = self.listener.accept()?;
        self.handle_stream(stream)
    }

    fn handle_stream(&self, stream: UnixStream) -> AppResult<()> {
        let mut writer = stream.try_clone()?;
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let response = self.handle_line(&line);
            serde_json::to_writer(&mut writer, &response)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        Ok(())
    }

    fn handle_line(&self, line: &str) -> Envelope {
        match decode_request(line) {
            Ok(request) => self.handle_request(request),
            Err(error) => *error,
        }
    }

    fn handle_request(&self, request: Envelope) -> Envelope {
        match request.method.as_deref() {
            Some("hello") => self.handle_hello(request),
            Some("run.start") => self.handle_run_start(request),
            Some(method) => Envelope::error(
                request.id,
                Some(method.into()),
                ERROR_UNSUPPORTED_METHOD,
                format!("unsupported method: {method}"),
            ),
            None => Envelope::error(
                request.id,
                None,
                ERROR_MALFORMED_REQUEST,
                "request method is required",
            ),
        }
    }

    fn handle_hello(&self, request: Envelope) -> Envelope {
        let params = match request.params {
            Some(params) => match serde_json::from_value::<HelloParams>(params) {
                Ok(params) => params,
                Err(error) => {
                    return Envelope::error(
                        request.id,
                        Some("hello".into()),
                        ERROR_MALFORMED_REQUEST,
                        format!("hello params are invalid: {error}"),
                    );
                }
            },
            None => {
                return Envelope::error(
                    request.id,
                    Some("hello".into()),
                    ERROR_MALFORMED_REQUEST,
                    "hello params are required",
                );
            }
        };

        if params.workspace_id != self.paths.workspace_id {
            return Envelope::error(
                request.id,
                Some("hello".into()),
                ERROR_WORKSPACE_MISMATCH,
                format!(
                    "workspace_id mismatch: expected {}, got {}",
                    self.paths.workspace_id, params.workspace_id
                ),
            );
        }

        match PathBuf::from(&params.workspace_root).canonicalize() {
            Ok(root) if root == self.paths.workspace_root => {}
            Ok(root) => {
                return Envelope::error(
                    request.id,
                    Some("hello".into()),
                    ERROR_WORKSPACE_MISMATCH,
                    format!(
                        "workspace_root mismatch: expected {}, got {}",
                        self.paths.workspace_root.display(),
                        root.display()
                    ),
                );
            }
            Err(error) => {
                return Envelope::error(
                    request.id,
                    Some("hello".into()),
                    ERROR_WORKSPACE_MISMATCH,
                    format!("workspace_root cannot be resolved: {error}"),
                );
            }
        }

        Envelope::response(
            request.id,
            Some("hello".into()),
            serde_json::to_value(HelloResult {
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                workspace_id: self.paths.workspace_id.clone(),
                ledger_path: self.paths.ledger_path.to_string_lossy().into_owned(),
                capabilities: vec!["hello".into(), "run.start".into()],
            })
            .expect("hello result serializes"),
        )
    }

    fn handle_run_start(&self, request: Envelope) -> Envelope {
        let params = match request.params {
            Some(params) => match serde_json::from_value::<RunStartParams>(params) {
                Ok(params) => params,
                Err(error) => {
                    return Envelope::error(
                        request.id,
                        Some("run.start".into()),
                        ERROR_MALFORMED_REQUEST,
                        format!("run.start params are invalid: {error}"),
                    );
                }
            },
            None => {
                return Envelope::error(
                    request.id,
                    Some("run.start".into()),
                    ERROR_MALFORMED_REQUEST,
                    "run.start params are required",
                );
            }
        };

        match run_question(RunOptions {
            question: params.question,
            config_path: self.resolve_config_path(params.config_path),
            ledger: RunLedger::Sqlite(self.paths.ledger_path.clone()),
            workspace_root: self.paths.workspace_root.clone(),
            approval_mode: ApprovalMode::Deny { actor: "daemon" },
        }) {
            Ok(outcome) => Envelope::response(
                request.id,
                Some("run.start".into()),
                serde_json::to_value(RunStartResult {
                    run_id: outcome.run_id.to_string(),
                    ledger_path: self.paths.ledger_path.to_string_lossy().into_owned(),
                    final_answer: outcome.final_answer,
                })
                .expect("run.start result serializes"),
            ),
            Err(error) => Envelope::error(
                request.id,
                Some("run.start".into()),
                ERROR_RUN_FAILED,
                error.to_string(),
            ),
        }
    }

    fn resolve_config_path(&self, config_path: Option<String>) -> PathBuf {
        match config_path {
            Some(path) => {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    self.paths.workspace_root.join(path)
                }
            }
            None => self.paths.workspace_root.join("plato.toml"),
        }
    }
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.paths.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{EnvelopeKind, ProtocolError};
    use std::{io::Read, thread};

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
            serde_json::json!(["hello", "run.start"])
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
}
