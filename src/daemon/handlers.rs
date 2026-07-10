use crate::{
    AppResult, ApprovalMode, RunEvent, RunLedger, RunOptions, RunSession,
    daemon::{
        protocol::{
            ApprovalDecideParams, CommandAcceptedResult, ERROR_LAGGED, ERROR_MALFORMED_REQUEST,
            ERROR_NOT_FOUND, ERROR_OVERLOAD, ERROR_RUN_FAILED, ERROR_SESSIONS_LIST_FAILED,
            ERROR_UNSUPPORTED_METHOD, ERROR_WORKSPACE_MISMATCH, Envelope, EventsStreamParams,
            EventsStreamResult, HelloParams, HelloResult, MessageAppendParams, RunCancelParams,
            RunStartParams, RunStartResult, SessionSummary, SessionsListResult,
            TranscriptReadParams, TranscriptReadResult, decode_request,
        },
        runtime::{DaemonRuntime, RunRecord, approval_handler},
    },
    ledger::SqliteLedger,
    new_run_id, new_session_id, replay_sqlite, replay_sqlite_session, run_question,
    tools::ApprovalOutcome,
};
use serde_json::{json, to_value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering, mpsc},
    thread,
};

const DEFAULT_EVENT_LIMIT: usize = 64;
const MAX_EVENT_LIMIT: usize = 128;
const SESSION_STATUS_RUNNING: &str = "running";
const SESSION_STATUS_INTERRUPTED: &str = "interrupted";
const LATEST_QUESTION_MAX_CHARS: usize = 120;

pub(super) fn handle_line(runtime: &DaemonRuntime, line: &str) -> Envelope {
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
        to_value(HelloResult {
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
        RunSession::Fresh {
            session_id: new_session_id(),
        },
        params.config_path,
        params.wait,
    )
}

fn handle_message_append(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let params = match decode_params::<MessageAppendParams>(&request, "message.append") {
        Ok(params) => params,
        Err(error) => return *error,
    };
    let session_id = match params.session_id {
        Some(session_id) => session_id,
        None => match latest_session_id(runtime) {
            Ok(session_id) => session_id,
            Err(error) => {
                return Envelope::error(
                    request.id,
                    Some("message.append".into()),
                    ERROR_NOT_FOUND,
                    error,
                );
            }
        },
    };
    start_run(
        runtime,
        request.id,
        "message.append",
        params.message,
        RunSession::Continue { session_id },
        params.config_path,
        params.wait,
    )
}

fn start_run(
    runtime: &DaemonRuntime,
    request_id: Option<String>,
    method: &'static str,
    question: String,
    session: RunSession,
    config_path: Option<String>,
    wait: Option<bool>,
) -> Envelope {
    let session_id = session.session_id().to_string();
    let mut runs = runtime.runs.lock().expect("runs lock poisoned");
    if let Some(active_run_id) = runs
        .values()
        .find(|record| {
            record.session_id == session_id
                && record.status().state == crate::daemon::runtime::RunStateName::Running
        })
        .map(|record| record.run_id.clone())
    {
        return Envelope::error(
            request_id,
            Some(method.into()),
            ERROR_OVERLOAD,
            format!("session already has an active run: {session_id} ({active_run_id})"),
        );
    }
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
        session_id,
        runtime.paths.ledger_path.clone(),
    ));
    runs.insert(run_id_string.clone(), record.clone());
    drop(runs);

    let (event_sender, event_receiver) = mpsc::channel::<RunEvent>();
    spawn_event_collector(record.clone(), event_receiver);
    let options = RunOptions {
        question,
        config_path: config_path.map(PathBuf::from),
        ledger: RunLedger::Sqlite(runtime.paths.ledger_path.clone()),
        workspace_root: runtime.paths.workspace_root.clone(),
        approval_mode: ApprovalMode::external("daemon", approval_handler(record.clone())),
        run_id: Some(run_id),
        session: Some(session),
        event_sender: Some(event_sender),
        stream_to_stderr: false,
        cancel: Some(record.cancel.clone()),
    };

    if wait.unwrap_or(false) {
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
        to_value(RunStartResult {
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
    let buffer = record.events.lock().expect("event buffer lock poisoned");
    let from_offset = params.from_offset.unwrap_or(buffer.next_offset);
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
    let next_offset = from_offset + events.len() as u64;
    Envelope::response(
        request.id,
        Some("events.stream".into()),
        to_value(EventsStreamResult {
            run_id: record.run_id.clone(),
            from_offset,
            next_offset,
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
        to_value(CommandAcceptedResult {
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
    let approvals = record.approvals.lock().expect("approvals lock poisoned");
    record.cancel.store(true, Ordering::SeqCst);
    record.push_event(json!({
        "kind": "canceled",
        "run_id": record.run_id,
    }));
    record.approval_changed.notify_all();
    drop(approvals);
    Envelope::response(
        request.id,
        Some("run.cancel".into()),
        to_value(CommandAcceptedResult {
            run_id: record.run_id.clone(),
            status: "cancel_requested".into(),
        })
        .expect("run.cancel result serializes"),
    )
}

fn handle_sessions_list(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    match session_summaries(runtime) {
        Ok(sessions) => Envelope::response(
            request.id,
            Some("sessions.list".into()),
            to_value(SessionsListResult { sessions }).expect("sessions.list result serializes"),
        ),
        Err(error) => Envelope::error(
            request.id,
            Some("sessions.list".into()),
            ERROR_SESSIONS_LIST_FAILED,
            error.to_string(),
        ),
    }
}

fn session_summaries(runtime: &DaemonRuntime) -> crate::AppResult<Vec<SessionSummary>> {
    let ledger_path = runtime.paths.ledger_path.clone();
    let mut sessions = crate::ledger::sqlite_session_summaries(&ledger_path)?
        .into_iter()
        .map(|session| SessionSummary {
            session_id: session.session_id,
            run_id: session.run_id,
            status: session.status,
            latest_question: latest_question_preview(&session.latest_question),
            ledger_path: ledger_path.to_string_lossy().into_owned(),
        })
        .collect::<Vec<_>>();

    let active_sessions = runtime
        .runs
        .lock()
        .expect("runs lock poisoned")
        .values()
        .filter_map(|record| {
            let status = record.status();
            if status.state != crate::daemon::runtime::RunStateName::Running {
                return None;
            }
            Some(SessionSummary {
                session_id: record.session_id.clone(),
                run_id: record.run_id.clone(),
                status: status.state.as_str().into(),
                latest_question: String::new(),
                ledger_path: record.ledger_path.to_string_lossy().into_owned(),
            })
        })
        .collect::<Vec<_>>();

    for session in &mut sessions {
        if session.status == SESSION_STATUS_RUNNING
            && !active_sessions
                .iter()
                .any(|active| active.session_id == session.session_id)
        {
            session.status = SESSION_STATUS_INTERRUPTED.into();
        }
    }

    for summary in active_sessions {
        if let Some(existing) = sessions
            .iter_mut()
            .find(|session| session.session_id == summary.session_id)
        {
            existing.run_id = summary.run_id;
            existing.status = summary.status;
            existing.ledger_path = summary.ledger_path;
        } else {
            // wait=false runs can be visible before begin_session_run persists the question.
            sessions.insert(0, summary);
        }
    }

    Ok(sessions)
}

fn latest_question_preview(question: &str) -> String {
    let line = question.lines().next().unwrap_or_default();
    if line.chars().count() <= LATEST_QUESTION_MAX_CHARS {
        return line.to_owned();
    }
    format!(
        "{}...",
        line.chars()
            .take(LATEST_QUESTION_MAX_CHARS)
            .collect::<String>()
    )
}

fn handle_transcript_read(runtime: &DaemonRuntime, request: Envelope) -> Envelope {
    let params = match decode_params::<TranscriptReadParams>(&request, "transcript.read") {
        Ok(params) => params,
        Err(error) => return *error,
    };
    let transcript = if let Some(run_id) = params.run_id {
        read_run_transcript(&runtime.paths.ledger_path, &run_id)
    } else if let Some(session_id) = params.session_id {
        read_session_transcript(&runtime.paths.ledger_path, &session_id)
    } else {
        return Envelope::error(
            request.id,
            Some("transcript.read".into()),
            ERROR_MALFORMED_REQUEST,
            "run_id or session_id is required",
        );
    };
    match transcript {
        Ok(transcript) => Envelope::response(
            request.id,
            Some("transcript.read".into()),
            to_value(transcript).expect("transcript.read result serializes"),
        ),
        Err(error) => Envelope::error(
            request.id,
            Some("transcript.read".into()),
            ERROR_NOT_FOUND,
            error.to_string(),
        ),
    }
}

fn read_run_transcript(path: &Path, run_id: &str) -> AppResult<TranscriptReadResult> {
    let status = SqliteLedger::open_readonly(path)?.run_status(run_id)?;
    Ok(TranscriptReadResult {
        run_id: status.run_id,
        status: status.status,
        final_answer: status.final_answer,
        transcript: replay_sqlite(path, Some(run_id))?,
    })
}

fn read_session_transcript(path: &Path, session_id: &str) -> AppResult<TranscriptReadResult> {
    let status = SqliteLedger::open_readonly(path)?.latest_session_run_status(session_id)?;
    Ok(TranscriptReadResult {
        run_id: status.run_id,
        status: status.status,
        final_answer: status.final_answer,
        transcript: replay_sqlite_session(path, session_id)?,
    })
}

fn latest_session_id(runtime: &DaemonRuntime) -> Result<String, String> {
    crate::ledger::latest_sqlite_session_id(&runtime.paths.ledger_path).map_err(|error| match error
    {
        crate::AppError::NoSqliteSessions | crate::AppError::NoSqliteRuns => {
            "no previous session exists".into()
        }
        error => error.to_string(),
    })
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

fn spawn_event_collector(record: Arc<RunRecord>, receiver: mpsc::Receiver<RunEvent>) {
    thread::spawn(move || {
        for event in receiver {
            match event {
                RunEvent::Ledger(recorded) => record.push_recorded_event(recorded),
                RunEvent::AssistantDelta(delta) => record.push_assistant_delta(delta),
            }
        }
    });
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
