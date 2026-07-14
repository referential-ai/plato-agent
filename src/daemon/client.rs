use crate::{
    AppError, AppResult,
    daemon::{
        protocol::{
            ApprovalDecideParams, CommandAcceptedResult, Envelope, EnvelopeKind,
            EventsStreamParams, EventsStreamResult, HelloParams, HelloResult, MessageAppendParams,
            PROTOCOL_VERSION, RunCancelParams, RunStartParams, RunStartResult, SessionSummary,
            SessionsListResult, ShutdownIfIdleResult, TranscriptReadParams, TranscriptReadResult,
        },
        transport::{self, Stream},
    },
    paths,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

pub struct DaemonClient {
    reader: BufReader<Stream>,
    writer: Stream,
    next_id: u64,
}

impl DaemonClient {
    pub fn connect(socket_path: &Path) -> AppResult<Self> {
        let writer = transport::connect(socket_path)?;
        let reader = BufReader::new(transport::try_clone(&writer)?);
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

    pub fn shutdown_if_idle(&mut self) -> AppResult<ShutdownIfIdleResult> {
        self.request_without_params("daemon.shutdown_if_idle")
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

    pub fn transcript_read_session(&mut self, session_id: &str) -> AppResult<TranscriptReadResult> {
        self.request(
            "transcript.read",
            TranscriptReadParams {
                run_id: None,
                session_id: Some(session_id.into()),
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
        self.message_append_to_session(message, None, config_path, wait)
    }

    pub fn message_append_to_session(
        &mut self,
        message: String,
        session_id: Option<String>,
        config_path: Option<String>,
        wait: bool,
    ) -> AppResult<RunStartResult> {
        self.request(
            "message.append",
            MessageAppendParams {
                message,
                session_id,
                config_path,
                wait: Some(wait),
            },
        )
    }

    pub fn events_stream(
        &mut self,
        run_id: &str,
        from_offset: Option<u64>,
        limit: usize,
    ) -> AppResult<EventsStreamResult> {
        self.request(
            "events.stream",
            EventsStreamParams {
                run_id: run_id.into(),
                from_offset,
                limit: Some(limit),
            },
        )
    }

    pub fn approval_grant(
        &mut self,
        run_id: &str,
        tool_call_id: &str,
    ) -> AppResult<CommandAcceptedResult> {
        self.request(
            "approval.decide",
            ApprovalDecideParams {
                run_id: run_id.into(),
                tool_call_id: tool_call_id.into(),
                decision: "grant".into(),
                reason: None,
            },
        )
    }

    pub fn approval_deny(
        &mut self,
        run_id: &str,
        tool_call_id: &str,
        reason: String,
    ) -> AppResult<CommandAcceptedResult> {
        self.request(
            "approval.decide",
            ApprovalDecideParams {
                run_id: run_id.into(),
                tool_call_id: tool_call_id.into(),
                decision: "deny".into(),
                reason: Some(reason),
            },
        )
    }

    pub fn run_cancel(&mut self, run_id: &str) -> AppResult<CommandAcceptedResult> {
        self.request(
            "run.cancel",
            RunCancelParams {
                run_id: run_id.into(),
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
        if response.v != PROTOCOL_VERSION {
            return Err(AppError::DaemonProtocol(format!(
                "unsupported response protocol version: {}",
                response.v
            )));
        }
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
                Err(AppError::DaemonResponse(error))
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::daemon::protocol::{
        ProtocolError, RunStateName, SessionSummary, ShutdownIfIdleResultName,
    };
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::{UnixListener, UnixStream},
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
                        "latest_question": "hello",
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
                status: RunStateName::Finished,
                latest_question: "hello".into(),
                ledger_path: "/tmp/agent.db".into(),
            }]
        );
    }

    #[test]
    fn client_omits_shutdown_params_and_decodes_both_outcomes() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            for outcome in ["shutdown", "refused_active"] {
                let request = read_request(&mut reader);
                assert_eq!(request.method.as_deref(), Some("daemon.shutdown_if_idle"));
                assert!(request.params.is_none());
                write_response(
                    &mut writer,
                    request.id,
                    "daemon.shutdown_if_idle",
                    json!({"result": outcome}),
                );
            }
        });

        let mut client = DaemonClient::connect(&socket_path).unwrap();
        assert_eq!(
            client.shutdown_if_idle().unwrap().result,
            ShutdownIfIdleResultName::Shutdown
        );
        assert_eq!(
            client.shutdown_if_idle().unwrap().result,
            ShutdownIfIdleResultName::RefusedActive
        );
        handle.join().unwrap();
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

        assert!(matches!(
            error,
            AppError::DaemonResponse(ProtocolError { code, message })
                if code == "not_found" && message == "missing"
        ));
    }

    #[test]
    fn client_rejects_an_unsupported_response_protocol_version() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let request = read_request(&mut reader);
            let mut response = Envelope::response(
                request.id,
                Some("sessions.list".into()),
                json!({"sessions": []}),
            );
            response.v = PROTOCOL_VERSION + 1;
            serde_json::to_writer(&mut writer, &response).unwrap();
            writer.write_all(b"\n").unwrap();
        });

        let mut client = DaemonClient::connect(&socket_path).unwrap();
        let error = client.sessions_list().unwrap_err();
        handle.join().unwrap();

        assert!(matches!(
            error,
            AppError::DaemonProtocol(message)
                if message == "unsupported response protocol version: 2"
        ));
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

            let tail = read_request(&mut reader);
            assert_eq!(tail.method.as_deref(), Some("events.stream"));
            assert!(tail.params.as_ref().unwrap().get("from_offset").is_none());
            write_response(
                &mut writer,
                tail.id,
                "events.stream",
                json!({
                    "run_id": "run_1",
                    "from_offset": 3,
                    "next_offset": 3,
                    "status": "finished",
                    "events": []
                }),
            );
        });

        let mut client = DaemonClient::connect(&socket_path).unwrap();
        let run = client
            .run_start("summarize this".into(), Some("plato.toml".into()), false)
            .unwrap();
        let events = client.events_stream(&run.run_id, Some(2), 16).unwrap();
        let tail = client.events_stream(&run.run_id, None, 16).unwrap();
        handle.join().unwrap();

        assert_eq!(run.run_id, "run_1");
        assert_eq!(events.next_offset, 3);
        assert_eq!(events.events.len(), 1);
        assert_eq!(tail.from_offset, 3);
        assert!(tail.events.is_empty());
    }

    #[test]
    fn client_sends_session_transcript_and_message_append_requests() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);

            let transcript = read_request(&mut reader);
            assert_eq!(transcript.method.as_deref(), Some("transcript.read"));
            assert!(transcript.params.as_ref().unwrap()["run_id"].is_null());
            assert_eq!(
                transcript.params.as_ref().unwrap()["session_id"],
                "session_1"
            );
            write_response(
                &mut writer,
                transcript.id,
                "transcript.read",
                json!({
                    "run_id": "run_1",
                    "status": "finished",
                    "final_answer": "hello",
                    "transcript": "[turn_1] user: hello"
                }),
            );

            let append = read_request(&mut reader);
            assert_eq!(append.method.as_deref(), Some("message.append"));
            assert_eq!(append.params.as_ref().unwrap()["message"], "follow up");
            assert_eq!(append.params.as_ref().unwrap()["session_id"], "session_1");
            assert_eq!(append.params.as_ref().unwrap()["wait"], false);
            write_response(
                &mut writer,
                append.id,
                "message.append",
                json!({
                    "run_id": "run_2",
                    "session_id": "session_1",
                    "ledger_path": "/tmp/agent.db",
                    "status": "running",
                    "final_answer": null
                }),
            );
        });

        let mut client = DaemonClient::connect(&socket_path).unwrap();
        let transcript = client.transcript_read_session("session_1").unwrap();
        let run = client
            .message_append_to_session(
                "follow up".into(),
                Some("session_1".into()),
                Some("plato.toml".into()),
                false,
            )
            .unwrap();
        handle.join().unwrap();

        assert_eq!(transcript.run_id, "run_1");
        assert_eq!(transcript.status, RunStateName::Finished);
        assert_eq!(transcript.final_answer.as_deref(), Some("hello"));
        assert_eq!(transcript.typed, None);
        assert_eq!(run.session_id, "session_1");
        assert_eq!(run.run_id, "run_2");
    }

    #[test]
    fn client_sends_approval_decisions_and_cancel_requests() {
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);

            let grant = read_request(&mut reader);
            assert_eq!(grant.method.as_deref(), Some("approval.decide"));
            assert_eq!(grant.params.as_ref().unwrap()["run_id"], "run_1");
            assert_eq!(grant.params.as_ref().unwrap()["tool_call_id"], "call_1");
            assert_eq!(grant.params.as_ref().unwrap()["decision"], "grant");
            assert!(grant.params.as_ref().unwrap()["reason"].is_null());
            write_response(
                &mut writer,
                grant.id,
                "approval.decide",
                json!({"run_id": "run_1", "status": "running"}),
            );

            let deny = read_request(&mut reader);
            assert_eq!(deny.method.as_deref(), Some("approval.decide"));
            assert_eq!(deny.params.as_ref().unwrap()["run_id"], "run_2");
            assert_eq!(deny.params.as_ref().unwrap()["tool_call_id"], "call_2");
            assert_eq!(deny.params.as_ref().unwrap()["decision"], "deny");
            assert_eq!(
                deny.params.as_ref().unwrap()["reason"],
                "denied by plato-tui"
            );
            write_response(
                &mut writer,
                deny.id,
                "approval.decide",
                json!({"run_id": "run_2", "status": "running"}),
            );

            let cancel = read_request(&mut reader);
            assert_eq!(cancel.method.as_deref(), Some("run.cancel"));
            assert_eq!(cancel.params.as_ref().unwrap()["run_id"], "run_3");
            write_response(
                &mut writer,
                cancel.id,
                "run.cancel",
                json!({"run_id": "run_3", "status": "cancel_requested"}),
            );
        });

        let mut client = DaemonClient::connect(&socket_path).unwrap();
        let granted = client.approval_grant("run_1", "call_1").unwrap();
        let denied = client
            .approval_deny("run_2", "call_2", "denied by plato-tui".into())
            .unwrap();
        let canceled = client.run_cancel("run_3").unwrap();
        handle.join().unwrap();

        assert_eq!(granted.status, RunStateName::Running);
        assert_eq!(denied.status, RunStateName::Running);
        assert_eq!(canceled.status, RunStateName::CancelRequested);
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
