use crate::{
    AppError, AppResult,
    daemon::protocol::{
        Envelope, EnvelopeKind, EventsStreamParams, EventsStreamResult, HelloParams, HelloResult,
        MessageAppendParams, PROTOCOL_VERSION, RunStartParams, RunStartResult, SessionSummary,
        SessionsListResult, TranscriptReadParams, TranscriptReadResult,
    },
    paths,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

pub struct DaemonClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl DaemonClient {
    pub fn connect(socket_path: &Path) -> AppResult<Self> {
        let writer = UnixStream::connect(socket_path)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self {
            reader,
            writer,
            next_id: 1,
        })
    }

    pub fn hello(&mut self, workspace_root: &Path) -> AppResult<HelloResult> {
        let workspace_root = workspace_root.canonicalize()?;
        let workspace_id = paths::workspace_id(&workspace_root)?;
        self.request(
            "hello",
            HelloParams {
                workspace_root: workspace_root.to_string_lossy().into_owned(),
                workspace_id,
            },
        )
    }

    pub fn sessions_list(&mut self) -> AppResult<Vec<SessionSummary>> {
        let result: SessionsListResult = self.request_without_params("sessions.list")?;
        Ok(result.sessions)
    }

    pub fn transcript_read(&mut self, run_id: &str) -> AppResult<TranscriptReadResult> {
        self.request(
            "transcript.read",
            TranscriptReadParams {
                run_id: Some(run_id.into()),
                session_id: None,
            },
        )
    }

    pub fn run_start(
        &mut self,
        question: String,
        config_path: Option<String>,
        wait: bool,
    ) -> AppResult<RunStartResult> {
        self.request(
            "run.start",
            RunStartParams {
                question,
                config_path,
                wait: Some(wait),
            },
        )
    }

    pub fn message_append(
        &mut self,
        message: String,
        config_path: Option<String>,
        wait: bool,
    ) -> AppResult<RunStartResult> {
        self.request(
            "message.append",
            MessageAppendParams {
                message,
                session_id: None,
                config_path,
                wait: Some(wait),
            },
        )
    }

    pub fn events_stream(
        &mut self,
        run_id: &str,
        from_offset: u64,
        limit: usize,
    ) -> AppResult<EventsStreamResult> {
        self.request(
            "events.stream",
            EventsStreamParams {
                run_id: run_id.into(),
                from_offset: Some(from_offset),
                limit: Some(limit),
            },
        )
    }

    fn request_without_params<T>(&mut self, method: &str) -> AppResult<T>
    where
        T: DeserializeOwned,
    {
        self.request_value(method, None)
    }

    fn request<T, P>(&mut self, method: &str, params: P) -> AppResult<T>
    where
        T: DeserializeOwned,
        P: Serialize,
    {
        self.request_value(method, Some(serde_json::to_value(params)?))
    }

    fn request_value<T>(&mut self, method: &str, params: Option<Value>) -> AppResult<T>
    where
        T: DeserializeOwned,
    {
        let id = self.next_request_id(method);
        let envelope = Envelope {
            v: PROTOCOL_VERSION,
            id: Some(id.clone()),
            kind: EnvelopeKind::Request,
            method: Some(method.into()),
            params,
            result: None,
            error: None,
        };
        serde_json::to_writer(&mut self.writer, &envelope)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(AppError::DaemonProtocol(
                "daemon connection closed before response".into(),
            ));
        }
        let response = serde_json::from_str::<Envelope>(line.trim())?;
        if response.id.as_deref() != Some(&id) {
            return Err(AppError::DaemonProtocol(format!(
                "response id mismatch: expected {id}, got {:?}",
                response.id
            )));
        }
        match response.kind {
            EnvelopeKind::Response => {
                let result = response.result.ok_or_else(|| {
                    AppError::DaemonProtocol(format!("{method} response missing result"))
                })?;
                Ok(serde_json::from_value(result)?)
            }
            EnvelopeKind::Error => {
                let error = response.error.ok_or_else(|| {
                    AppError::DaemonProtocol(format!("{method} error missing payload"))
                })?;
                Err(AppError::DaemonProtocol(format!(
                    "{}: {}",
                    error.code, error.message
                )))
            }
            other => Err(AppError::DaemonProtocol(format!(
                "{method} returned unexpected envelope kind {other:?}"
            ))),
        }
    }

    fn next_request_id(&mut self, method: &str) -> String {
        let id = format!("{}_{}", method.replace('.', "_"), self.next_id);
        self.next_id += 1;
        id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonConnectionConfig {
    pub workspace_root: PathBuf,
    pub socket_path: PathBuf,
}

impl DaemonConnectionConfig {
    pub fn resolve(workspace_root: &Path, socket_path: Option<PathBuf>) -> AppResult<Self> {
        let workspace_root = workspace_root.canonicalize()?;
        let socket_path = socket_path.unwrap_or(paths::default_socket_path(&workspace_root)?);
        Ok(Self {
            workspace_root,
            socket_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{ProtocolError, SessionSummary};
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        thread,
    };

    #[test]
    fn client_sends_hello_and_sessions_requests() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let workspace_id = paths::workspace_id(workspace.path()).unwrap();
        let workspace_root = workspace.path().canonicalize().unwrap();
        let expected_id = workspace_id.clone();
        let expected_root = workspace_root.to_string_lossy().into_owned();

        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);

            let hello = read_request(&mut reader);
            assert_eq!(hello.method.as_deref(), Some("hello"));
            assert_eq!(hello.params.as_ref().unwrap()["workspace_id"], expected_id);
            assert_eq!(
                hello.params.as_ref().unwrap()["workspace_root"],
                expected_root
            );
            write_response(
                &mut writer,
                hello.id,
                "hello",
                json!({
                    "daemon_version": "0.1.0",
                    "workspace_id": expected_id,
                    "ledger_path": "/tmp/agent.db",
                    "capabilities": ["hello", "sessions.list"]
                }),
            );

            let sessions = read_request(&mut reader);
            assert_eq!(sessions.method.as_deref(), Some("sessions.list"));
            write_response(
                &mut writer,
                sessions.id,
                "sessions.list",
                json!({
                    "sessions": [{
                        "session_id": "run_1",
                        "run_id": "run_1",
                        "status": "finished",
                        "ledger_path": "/tmp/agent.db"
                    }]
                }),
            );
        });

        let mut client = DaemonClient::connect(&socket_path).unwrap();
        let hello = client.hello(&workspace_root).unwrap();
        let sessions = client.sessions_list().unwrap();
        handle.join().unwrap();

        assert_eq!(hello.workspace_id, workspace_id);
        assert_eq!(
            sessions,
            vec![SessionSummary {
                session_id: "run_1".into(),
                run_id: "run_1".into(),
                status: "finished".into(),
                ledger_path: "/tmp/agent.db".into(),
            }]
        );
    }

    #[test]
    fn client_maps_protocol_errors() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let request = read_request(&mut reader);
            let response = Envelope {
                v: PROTOCOL_VERSION,
                id: request.id,
                kind: EnvelopeKind::Error,
                method: Some("sessions.list".into()),
                params: None,
                result: None,
                error: Some(ProtocolError {
                    code: "not_found".into(),
                    message: "missing".into(),
                }),
            };
            serde_json::to_writer(&mut writer, &response).unwrap();
            writer.write_all(b"\n").unwrap();
        });

        let mut client = DaemonClient::connect(&socket_path).unwrap();
        let error = client.sessions_list().unwrap_err();
        handle.join().unwrap();

        assert!(error.to_string().contains("not_found: missing"));
    }

    #[test]
    fn client_sends_run_start_and_events_stream_requests() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);

            let run_start = read_request(&mut reader);
            assert_eq!(run_start.method.as_deref(), Some("run.start"));
            assert_eq!(
                run_start.params.as_ref().unwrap()["question"],
                "summarize this"
            );
            assert_eq!(run_start.params.as_ref().unwrap()["wait"], false);
            write_response(
                &mut writer,
                run_start.id,
                "run.start",
                json!({
                    "run_id": "run_1",
                    "session_id": "run_1",
                    "ledger_path": "/tmp/agent.db",
                    "status": "running",
                    "final_answer": null
                }),
            );

            let events = read_request(&mut reader);
            assert_eq!(events.method.as_deref(), Some("events.stream"));
            assert_eq!(events.params.as_ref().unwrap()["run_id"], "run_1");
            assert_eq!(events.params.as_ref().unwrap()["from_offset"], 2);
            assert_eq!(events.params.as_ref().unwrap()["limit"], 16);
            write_response(
                &mut writer,
                events.id,
                "events.stream",
                json!({
                    "run_id": "run_1",
                    "from_offset": 2,
                    "next_offset": 3,
                    "status": "running",
                    "events": [{
                        "offset": 2,
                        "event": {"kind": "test"}
                    }]
                }),
            );
        });

        let mut client = DaemonClient::connect(&socket_path).unwrap();
        let run = client
            .run_start("summarize this".into(), Some("plato.toml".into()), false)
            .unwrap();
        let events = client.events_stream(&run.run_id, 2, 16).unwrap();
        handle.join().unwrap();

        assert_eq!(run.run_id, "run_1");
        assert_eq!(events.next_offset, 3);
        assert_eq!(events.events.len(), 1);
    }

    fn read_request(reader: &mut BufReader<UnixStream>) -> Envelope {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    fn write_response(writer: &mut UnixStream, id: Option<String>, method: &str, result: Value) {
        let response = Envelope::response(id, Some(method.into()), result);
        serde_json::to_writer(writer.by_ref(), &response).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
    }
}
