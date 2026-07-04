use crate::{
    AppResult, ApprovalMode, ApprovalRequest, RunLedger, RunOptions,
    daemon::{
        lock::WorkspaceLock,
        protocol::{
            ApprovalDecideParams, CommandAcceptedResult, ERROR_LAGGED, ERROR_MALFORMED_REQUEST,
            ERROR_NOT_FOUND, ERROR_OVERLOAD, ERROR_RUN_FAILED, ERROR_UNSUPPORTED_METHOD,
            ERROR_WORKSPACE_MISMATCH, Envelope, EventsStreamParams, EventsStreamResult,
            HelloParams, HelloResult, MessageAppendParams, RunCancelParams, RunStartParams,
            RunStartResult, SessionSummary, SessionsListResult, TranscriptReadParams,
            TranscriptReadResult, decode_request,
        },
    },
    new_run_id, paths, replay_sqlite, run_question,
    tools::ApprovalOutcome,
};
use platonic_core::RecordedEvent;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

const MAX_EVENT_BUFFER: usize = 256;
const DEFAULT_EVENT_LIMIT: usize = 64;
const MAX_EVENT_LIMIT: usize = 128;

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

#[derive(Clone, Debug)]
struct DaemonRuntime {
    paths: DaemonPaths,
    runs: Arc<Mutex<HashMap<String, Arc<RunRecord>>>>,
}

#[derive(Debug)]
struct RunRecord {
    run_id: String,
    session_id: String,
    ledger_path: PathBuf,
    cancel: Arc<AtomicBool>,
    status: Mutex<RunStatus>,
    events: Mutex<EventBuffer>,
    approvals: Mutex<HashMap<String, PendingApproval>>,
    approval_changed: Condvar,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RunStatus {
    state: RunStateName,
    final_answer: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RunStateName {
    Running,
    Finished,
    Failed,
    Canceled,
}

impl RunStateName {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Debug)]
struct EventBuffer {
    first_offset: u64,
    next_offset: u64,
    events: VecDeque<Value>,
}

#[derive(Clone, Debug)]
struct PendingApproval {
    decision: Option<ApprovalOutcome>,
}

impl RunRecord {
    fn new(run_id: String, ledger_path: PathBuf) -> Self {
        Self {
            session_id: run_id.clone(),
            run_id,
            ledger_path,
            cancel: Arc::new(AtomicBool::new(false)),
            status: Mutex::new(RunStatus {
                state: RunStateName::Running,
                final_answer: None,
                error: None,
            }),
            events: Mutex::new(EventBuffer {
                first_offset: 0,
                next_offset: 0,
                events: VecDeque::new(),
            }),
            approvals: Mutex::new(HashMap::new()),
            approval_changed: Condvar::new(),
        }
    }

    fn push_event(&self, event: Value) {
        let mut buffer = self.events.lock().expect("event buffer lock poisoned");
        if buffer.events.len() == MAX_EVENT_BUFFER {
            buffer.events.pop_front();
            buffer.first_offset += 1;
        }
        let offset = buffer.next_offset;
        buffer.next_offset += 1;
        buffer.events.push_back(json!({
            "offset": offset,
            "event": event,
        }));
    }

    fn push_recorded_event(&self, record: RecordedEvent) {
        self.push_event(json!({
            "kind": "ledger",
            "record": record,
        }));
    }

    fn status(&self) -> RunStatus {
        self.status
            .lock()
            .expect("run status lock poisoned")
            .clone()
    }

    fn set_finished(&self, final_answer: String) {
        let mut status = self.status.lock().expect("run status lock poisoned");
        status.state = RunStateName::Finished;
        status.final_answer = Some(final_answer);
        status.error = None;
    }

    fn set_failed(&self, error: String) {
        let mut status = self.status.lock().expect("run status lock poisoned");
        status.state = if self.cancel.load(Ordering::SeqCst) {
            RunStateName::Canceled
        } else {
            RunStateName::Failed
        };
        status.final_answer = None;
        status.error = Some(error);
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
        let runtime = DaemonRuntime {
            paths,
            runs: Arc::new(Mutex::new(HashMap::new())),
        };
        Ok(Self {
            listener,
            runtime,
            _lock: lock,
        })
    }

    pub fn paths(&self) -> &DaemonPaths {
        &self.runtime.paths
    }

    pub fn serve_forever(&self) -> AppResult<()> {
        for stream in self.listener.incoming() {
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
    fn handle_line(&self, line: &str) -> Envelope {
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

fn handle_line(runtime: &DaemonRuntime, line: &str) -> Envelope {
    match decode_request(line) {
        Ok(request) => handle_request(runtime, request),
        Err(error) => *error,
    }
}

fn handle_request(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    match request.method.as_deref() {
        Some("hello") => handle_hello(runtime, request),
        Some("run.start") => handle_run_start(runtime, request),
        Some("message.append") => handle_message_append(runtime, request),
        Some("events.stream") => handle_events_stream(runtime, request),
        Some("approval.decide") => handle_approval_decide(runtime, request),
        Some("run.cancel") => handle_run_cancel(runtime, request),
        Some("sessions.list") => handle_sessions_list(runtime, request),
        Some("transcript.read") => handle_transcript_read(runtime, request),
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

fn handle_hello(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
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

    if params.workspace_id != runtime.paths.workspace_id {
        return Envelope::error(
            request.id,
            Some("hello".into()),
            ERROR_WORKSPACE_MISMATCH,
            format!(
                "workspace_id mismatch: expected {}, got {}",
                runtime.paths.workspace_id, params.workspace_id
            ),
        );
    }

    match PathBuf::from(&params.workspace_root).canonicalize() {
        Ok(root) if root == runtime.paths.workspace_root => {}
        Ok(root) => {
            return Envelope::error(
                request.id,
                Some("hello".into()),
                ERROR_WORKSPACE_MISMATCH,
                format!(
                    "workspace_root mismatch: expected {}, got {}",
                    runtime.paths.workspace_root.display(),
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
            workspace_id: runtime.paths.workspace_id.clone(),
            ledger_path: runtime.paths.ledger_path.to_string_lossy().into_owned(),
            capabilities: vec![
                "hello".into(),
                "run.start".into(),
                "message.append".into(),
                "events.stream".into(),
                "approval.decide".into(),
                "run.cancel".into(),
                "sessions.list".into(),
                "transcript.read".into(),
            ],
        })
        .expect("hello result serializes"),
    )
}

fn handle_run_start(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let params = match decode_params::<RunStartParams>(&request, "run.start") {
        Ok(params) => params,
        Err(error) => return *error,
    };
    start_run(
        runtime,
        request.id,
        "run.start",
        params.question,
        params.config_path,
        params.wait,
    )
}

fn handle_message_append(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let params = match decode_params::<MessageAppendParams>(&request, "message.append") {
        Ok(params) => params,
        Err(error) => return *error,
    };
    start_run(
        runtime,
        request.id,
        "message.append",
        params.message,
        params.config_path,
        params.wait,
    )
}

fn start_run(
    runtime: &DaemonRuntime,
    request_id: Option<String>,
    method: &'static str,
    question: String,
    config_path: Option<String>,
    wait: Option<bool>,
) -> Envelope {
    let run_id = match new_run_id() {
        Ok(run_id) => run_id,
        Err(error) => {
            return Envelope::error(
                request_id,
                Some(method.into()),
                ERROR_RUN_FAILED,
                error.to_string(),
            );
        }
    };
    let run_id_string = run_id.to_string();
    let record = Arc::new(RunRecord::new(
        run_id_string.clone(),
        runtime.paths.ledger_path.clone(),
    ));
    runtime
        .runs
        .lock()
        .expect("runs lock poisoned")
        .insert(run_id_string.clone(), record.clone());

    let (event_sender, event_receiver) = mpsc::channel::<RecordedEvent>();
    spawn_event_collector(record.clone(), event_receiver);
    let options = RunOptions {
        question,
        config_path: resolve_config_path(&runtime.paths, config_path),
        ledger: RunLedger::Sqlite(runtime.paths.ledger_path.clone()),
        workspace_root: runtime.paths.workspace_root.clone(),
        approval_mode: ApprovalMode::external("daemon", approval_handler(record.clone())),
        run_id: Some(run_id),
        event_sender: Some(event_sender),
        cancel: Some(record.cancel.clone()),
    };

    if wait.unwrap_or(true) {
        match run_question(options) {
            Ok(outcome) => {
                record.set_finished(outcome.final_answer.clone());
                run_start_response(request_id, method, &record)
            }
            Err(error) => {
                record.set_failed(error.to_string());
                Envelope::error(
                    request_id,
                    Some(method.into()),
                    ERROR_RUN_FAILED,
                    error.to_string(),
                )
            }
        }
    } else {
        let worker_record = record.clone();
        thread::spawn(move || match run_question(options) {
            Ok(outcome) => worker_record.set_finished(outcome.final_answer),
            Err(error) => worker_record.set_failed(error.to_string()),
        });
        run_start_response(request_id, method, &record)
    }
}

fn run_start_response(request_id: Option<String>, method: &str, record: &RunRecord) -> Envelope {
    let status = record.status();
    Envelope::response(
        request_id,
        Some(method.into()),
        serde_json::to_value(RunStartResult {
            run_id: record.run_id.clone(),
            session_id: record.session_id.clone(),
            ledger_path: record.ledger_path.to_string_lossy().into_owned(),
            status: status.state.as_str().into(),
            final_answer: status.final_answer,
        })
        .expect("run.start result serializes"),
    )
}

fn handle_events_stream(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let params = match decode_params::<EventsStreamParams>(&request, "events.stream") {
        Ok(params) => params,
        Err(error) => return *error,
    };
    let record = match find_run(runtime, &params.run_id) {
        Ok(record) => record,
        Err(error) => return error_response(request.id, "events.stream", error),
    };
    let limit = params.limit.unwrap_or(DEFAULT_EVENT_LIMIT);
    if limit > MAX_EVENT_LIMIT {
        return Envelope::error(
            request.id,
            Some("events.stream".into()),
            ERROR_OVERLOAD,
            format!("event stream limit exceeds maximum {MAX_EVENT_LIMIT}: {limit}"),
        );
    }
    let from_offset = params.from_offset.unwrap_or(0);
    let buffer = record.events.lock().expect("event buffer lock poisoned");
    if from_offset < buffer.first_offset {
        return Envelope::error(
            request.id,
            Some("events.stream".into()),
            ERROR_LAGGED,
            format!(
                "requested offset {from_offset} is no longer buffered; first available is {}",
                buffer.first_offset
            ),
        );
    }
    let start = (from_offset - buffer.first_offset) as usize;
    let events = buffer
        .events
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    Envelope::response(
        request.id,
        Some("events.stream".into()),
        serde_json::to_value(EventsStreamResult {
            run_id: record.run_id.clone(),
            from_offset,
            next_offset: buffer.next_offset,
            status: record.status().state.as_str().into(),
            events,
        })
        .expect("events.stream result serializes"),
    )
}

fn handle_approval_decide(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let params = match decode_params::<ApprovalDecideParams>(&request, "approval.decide") {
        Ok(params) => params,
        Err(error) => return *error,
    };
    let record = match find_run(runtime, &params.run_id) {
        Ok(record) => record,
        Err(error) => return error_response(request.id, "approval.decide", error),
    };
    let mut approvals = record.approvals.lock().expect("approvals lock poisoned");
    let pending = match approvals.get_mut(&params.tool_call_id) {
        Some(pending) => pending,
        None => {
            return Envelope::error(
                request.id,
                Some("approval.decide".into()),
                ERROR_NOT_FOUND,
                format!("pending approval not found: {}", params.tool_call_id),
            );
        }
    };
    pending.decision = Some(match params.decision.as_str() {
        "grant" => ApprovalOutcome::Granted,
        "deny" => ApprovalOutcome::Denied {
            reason: params
                .reason
                .unwrap_or_else(|| "approval denied by daemon client".into()),
        },
        other => {
            return Envelope::error(
                request.id,
                Some("approval.decide".into()),
                ERROR_MALFORMED_REQUEST,
                format!("approval decision must be grant or deny, got {other}"),
            );
        }
    });
    record.approval_changed.notify_all();
    drop(approvals);
    Envelope::response(
        request.id,
        Some("approval.decide".into()),
        serde_json::to_value(CommandAcceptedResult {
            run_id: record.run_id.clone(),
            status: record.status().state.as_str().into(),
        })
        .expect("approval.decide result serializes"),
    )
}

fn handle_run_cancel(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let params = match decode_params::<RunCancelParams>(&request, "run.cancel") {
        Ok(params) => params,
        Err(error) => return *error,
    };
    let record = match find_run(runtime, &params.run_id) {
        Ok(record) => record,
        Err(error) => return error_response(request.id, "run.cancel", error),
    };
    record.cancel.store(true, Ordering::SeqCst);
    record.push_event(json!({
        "kind": "canceled",
        "run_id": record.run_id,
    }));
    record.approval_changed.notify_all();
    Envelope::response(
        request.id,
        Some("run.cancel".into()),
        serde_json::to_value(CommandAcceptedResult {
            run_id: record.run_id.clone(),
            status: "cancel_requested".into(),
        })
        .expect("run.cancel result serializes"),
    )
}

fn handle_sessions_list(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let runs = runtime.runs.lock().expect("runs lock poisoned");
    let sessions = runs
        .values()
        .map(|record| SessionSummary {
            session_id: record.session_id.clone(),
            run_id: record.run_id.clone(),
            status: record.status().state.as_str().into(),
            ledger_path: record.ledger_path.to_string_lossy().into_owned(),
        })
        .collect();
    Envelope::response(
        request.id,
        Some("sessions.list".into()),
        serde_json::to_value(SessionsListResult { sessions })
            .expect("sessions.list result serializes"),
    )
}

fn handle_transcript_read(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let params = match decode_params::<TranscriptReadParams>(&request, "transcript.read") {
        Ok(params) => params,
        Err(error) => return *error,
    };
    let run_id = match params.run_id.or(params.session_id) {
        Some(run_id) => run_id,
        None => {
            return Envelope::error(
                request.id,
                Some("transcript.read".into()),
                ERROR_MALFORMED_REQUEST,
                "run_id or session_id is required",
            );
        }
    };
    match replay_sqlite(&runtime.paths.ledger_path, Some(&run_id)) {
        Ok(transcript) => Envelope::response(
            request.id,
            Some("transcript.read".into()),
            serde_json::to_value(TranscriptReadResult { run_id, transcript })
                .expect("transcript.read result serializes"),
        ),
        Err(error) => Envelope::error(
            request.id,
            Some("transcript.read".into()),
            ERROR_NOT_FOUND,
            error.to_string(),
        ),
    }
}

fn decode_params<T: serde::de::DeserializeOwned>(
    request: &Envelope,
    method: &'static str,
) -> Result<T, Box<Envelope>> {
    match &request.params {
        Some(params) => serde_json::from_value::<T>(params.clone()).map_err(|error| {
            Box::new(Envelope::error(
                request.id.clone(),
                Some(method.into()),
                ERROR_MALFORMED_REQUEST,
                format!("{method} params are invalid: {error}"),
            ))
        }),
        None => Err(Box::new(Envelope::error(
            request.id.clone(),
            Some(method.into()),
            ERROR_MALFORMED_REQUEST,
            format!("{method} params are required"),
        ))),
    }
}

fn resolve_config_path(paths: &DaemonPaths, config_path: Option<String>) -> PathBuf {
    match config_path {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                paths.workspace_root.join(path)
            }
        }
        None => paths.workspace_root.join("plato.toml"),
    }
}

fn spawn_event_collector(record: Arc<RunRecord>, receiver: mpsc::Receiver<RecordedEvent>) {
    thread::spawn(move || {
        for event in receiver {
            record.push_recorded_event(event);
        }
    });
}

fn approval_handler(
    record: Arc<RunRecord>,
) -> impl Fn(ApprovalRequest) -> AppResult<ApprovalOutcome> + Send + Sync + 'static {
    move |request| {
        record.push_event(json!({
            "kind": "approval_requested",
            "run_id": request.run_id,
            "tool_call_id": request.call_id,
            "tool_name": request.tool_name,
            "effect": request.effect,
            "reason": request.reason,
        }));
        let call_id = request.call_id.to_string();
        let mut approvals = record.approvals.lock().expect("approvals lock poisoned");
        approvals.insert(call_id.clone(), PendingApproval { decision: None });
        loop {
            if record.cancel.load(Ordering::SeqCst) {
                approvals.remove(&call_id);
                return Ok(ApprovalOutcome::Denied {
                    reason: "run canceled".into(),
                });
            }
            if let Some(pending) = approvals.get_mut(&call_id)
                && let Some(decision) = pending.decision.take()
            {
                approvals.remove(&call_id);
                return Ok(decision);
            }
            approvals = record
                .approval_changed
                .wait(approvals)
                .expect("approval condvar lock poisoned");
        }
    }
}

fn find_run(runtime: &DaemonRuntime, run_id: &str) -> Result<Arc<RunRecord>, String> {
    runtime
        .runs
        .lock()
        .expect("runs lock poisoned")
        .get(run_id)
        .cloned()
        .ok_or_else(|| format!("run not found: {run_id}"))
}

fn error_response(request_id: Option<String>, method: &'static str, message: String) -> Envelope {
    Envelope::error(request_id, Some(method.into()), ERROR_NOT_FOUND, message)
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.runtime.paths.socket_path);
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
            server.paths().ledger_path.clone(),
        ));
        record
            .approvals
            .lock()
            .unwrap()
            .insert("call_1".into(), PendingApproval { decision: None });
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
        assert_eq!(result["sessions"][0]["session_id"], "run_1");
        assert_eq!(result["sessions"][0]["run_id"], "run_1");
    }
}
