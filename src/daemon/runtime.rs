use crate::{AppResult, ApprovalRequest, daemon::server::DaemonPaths, tools::ApprovalOutcome};
use platonic_core::RecordedEvent;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) const MAX_EVENT_BUFFER: usize = 256;

#[derive(Clone, Debug)]
pub(super) struct DaemonRuntime {
    pub(super) paths: DaemonPaths,
    pub(super) runs: Arc<Mutex<HashMap<String, Arc<RunRecord>>>>,
}

impl DaemonRuntime {
    pub(super) fn new(paths: DaemonPaths) -> Self {
        Self {
            paths,
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Debug)]
pub(super) struct RunRecord {
    pub(super) run_id: String,
    pub(super) session_id: String,
    pub(super) ledger_path: PathBuf,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) status: Mutex<RunStatus>,
    pub(super) events: Mutex<EventBuffer>,
    pub(super) approvals: Mutex<HashMap<String, PendingApproval>>,
    pub(super) approval_changed: Condvar,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RunStatus {
    pub(super) state: RunStateName,
    pub(super) final_answer: Option<String>,
    pub(super) error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RunStateName {
    Running,
    Finished,
    Failed,
    Canceled,
}

impl RunStateName {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Debug)]
pub(super) struct EventBuffer {
    pub(super) first_offset: u64,
    pub(super) next_offset: u64,
    pub(super) events: VecDeque<Value>,
}

#[derive(Clone, Debug)]
pub(super) struct PendingApproval {
    pub(super) decision: Option<ApprovalOutcome>,
}

impl PendingApproval {
    pub(super) fn new() -> Self {
        Self { decision: None }
    }
}

impl RunRecord {
    pub(super) fn new(run_id: String, ledger_path: PathBuf) -> Self {
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

    pub(super) fn push_event(&self, event: Value) {
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

    pub(super) fn push_recorded_event(&self, record: RecordedEvent) {
        self.push_event(json!({
            "kind": "ledger",
            "record": record,
        }));
    }

    pub(super) fn status(&self) -> RunStatus {
        self.status
            .lock()
            .expect("run status lock poisoned")
            .clone()
    }

    pub(super) fn set_finished(&self, final_answer: String) {
        let mut status = self.status.lock().expect("run status lock poisoned");
        status.state = RunStateName::Finished;
        status.final_answer = Some(final_answer);
        status.error = None;
    }

    pub(super) fn set_failed(&self, error: String) {
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

pub(super) fn approval_handler(
    record: Arc<RunRecord>,
) -> impl Fn(ApprovalRequest) -> AppResult<ApprovalOutcome> + Send + Sync + 'static {
    move |request| {
        record.push_event(approval_requested_event(&request));
        let call_id = request.call_id.to_string();
        let mut approvals = record.approvals.lock().expect("approvals lock poisoned");
        approvals.insert(call_id.clone(), PendingApproval::new());
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

fn approval_requested_event(request: &ApprovalRequest) -> Value {
    let mut event = json!({
        "kind": "approval_requested",
        "run_id": &request.run_id,
        "tool_call_id": &request.call_id,
        "tool_name": &request.tool_name,
        "effect": &request.effect,
        "reason": &request.reason,
    });
    if let Some(diff_preview) = &request.diff_preview {
        event["diff_preview"] = json!(diff_preview);
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use platonic_core::{EffectClass, RunId, ToolCallId};

    #[test]
    fn approval_requested_event_carries_diff_preview_when_present() {
        let event = approval_requested_event(&ApprovalRequest {
            run_id: RunId::new("run_1").unwrap(),
            call_id: ToolCallId::new("call_1").unwrap(),
            tool_name: "file.edit".into(),
            effect: EffectClass::WorkspaceWrite,
            reason: "file.edit requires approval".into(),
            diff_preview: Some("--- a/note.txt\n+++ b/note.txt\n".into()),
        });

        assert_eq!(event["kind"], "approval_requested");
        assert_eq!(event["diff_preview"], "--- a/note.txt\n+++ b/note.txt\n");
    }

    #[test]
    fn approval_requested_event_omits_diff_preview_when_absent() {
        let event = approval_requested_event(&ApprovalRequest {
            run_id: RunId::new("run_1").unwrap(),
            call_id: ToolCallId::new("call_1").unwrap(),
            tool_name: "file.write".into(),
            effect: EffectClass::WorkspaceWrite,
            reason: "file.write requires approval".into(),
            diff_preview: None,
        });

        assert!(event.get("diff_preview").is_none());
    }
}
