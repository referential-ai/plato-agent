use crate::{
    ApprovalMode, RunLedger, RunOptions,
    daemon::{
        protocol::{
            ApprovalDecideParams, CommandAcceptedResult, ERROR_LAGGED, ERROR_MALFORMED_REQUEST,
            ERROR_NOT_FOUND, ERROR_OVERLOAD, ERROR_RUN_FAILED, ERROR_UNSUPPORTED_METHOD,
            ERROR_WORKSPACE_MISMATCH, Envelope, EventsStreamParams, EventsStreamResult,
            HelloParams, HelloResult, MessageAppendParams, RunCancelParams, RunStartParams,
            RunStartResult, SessionSummary, SessionsListResult, TranscriptReadParams,
            TranscriptReadResult, decode_request,
        },
        runtime::{DaemonRuntime, RunRecord, approval_handler},
    },
    new_run_id, replay_sqlite, run_question,
    tools::ApprovalOutcome,
};
use platonic_core::RecordedEvent;
use serde_json::{json, to_value};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering, mpsc},
    thread,
};

const DEFAULT_EVENT_LIMIT: usize = 64;
const MAX_EVENT_LIMIT: usize = 128;

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
        config_path: config_path.map(PathBuf::from),
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
        to_value(EventsStreamResult {
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
    record.cancel.store(true, Ordering::SeqCst);
    record.push_event(json!({
        "kind": "canceled",
        "run_id": record.run_id,
    }));
    record.approval_changed.notify_all();
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
        to_value(SessionsListResult { sessions }).expect("sessions.list result serializes"),
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
            to_value(TranscriptReadResult { run_id, transcript })
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

fn spawn_event_collector(record: Arc<RunRecord>, receiver: mpsc::Receiver<RecordedEvent>) {
    thread::spawn(move || {
        for event in receiver {
            record.push_recorded_event(event);
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
