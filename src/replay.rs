use crate::{AppError, AppResult, ledger};
use platonic_core::{MessageRole, ReadbackEntry, RunReadback};
use std::path::Path;

pub fn replay_file(path: &Path) -> AppResult<String> {
    let records = ledger::read_records(path)?;
    let readback = RunReadback::from_events(&records)?;
    Ok(format_readback(&readback))
}

pub fn replay_sqlite(path: &Path, run_id: Option<&str>) -> AppResult<String> {
    if let Some(run_id) = run_id {
        let records = ledger::read_sqlite_records(path, Some(run_id))?;
        let readback = RunReadback::from_events(&records)?;
        return Ok(format_readback(&readback));
    }

    match ledger::read_latest_sqlite_session(path) {
        Ok(session) => format_session_readback(&session),
        Err(AppError::NoSqliteSessions) => {
            let records = ledger::read_sqlite_records(path, None)?;
            let readback = RunReadback::from_events(&records)?;
            Ok(format_readback(&readback))
        }
        Err(error) => Err(error),
    }
}

pub fn replay_sqlite_session(path: &Path, session_id: &str) -> AppResult<String> {
    let session = ledger::read_sqlite_session(path, session_id)?;
    format_session_readback(&session)
}

pub fn format_readback(readback: &RunReadback) -> String {
    let mut lines = Vec::new();
    lines.push(format!("final_phase: {:?}", readback.final_phase));
    lines.push(format!("next_seq: {}", readback.next_seq));

    for entry in &readback.entries {
        match entry {
            ReadbackEntry::ContextFragment { turn_id, fragment } => {
                lines.push(format!(
                    "[{turn_id}] context {:?} {}: {}",
                    fragment.lane, fragment.source, fragment.content
                ));
            }
            ReadbackEntry::ModelMessage { turn_id, message } => {
                let role = match message.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Tool => "tool",
                };
                lines.push(format!("[{turn_id}] {role}: {}", message.content));
            }
            ReadbackEntry::ToolCall { turn_id, call } => {
                lines.push(format!(
                    "[{turn_id}] tool_call {} {}",
                    call.tool, call.input
                ));
            }
            ReadbackEntry::ToolResult { result } => {
                lines.push(format!(
                    "tool_result {}: {}",
                    result.call_id, result.summary
                ));
            }
            ReadbackEntry::PolicyDenied { call_id, reason } => {
                lines.push(format!("policy_denied {call_id}: {reason}"));
            }
            ReadbackEntry::ApprovalGranted { call_id, actor_id } => {
                lines.push(format!("approval_granted {call_id} by {actor_id}"));
            }
            ReadbackEntry::ApprovalDenied {
                call_id,
                actor_id,
                reason,
            } => {
                lines.push(format!("approval_denied {call_id} by {actor_id}: {reason}"));
            }
            ReadbackEntry::ToolFailed { call_id, reason } => {
                lines.push(format!("tool_failed {call_id}: {reason}"));
            }
        }
    }

    lines.join("\n")
}

fn format_session_readback(session: &ledger::SessionRecords) -> AppResult<String> {
    let mut lines = vec![format!("session_id: {}", session.session_id)];
    for run in &session.runs {
        lines.push(format!("run_id: {}", run.run_id));
        let readback = RunReadback::from_events(&run.records)?;
        lines.push(format_readback(&readback));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::SqliteLedger;
    use platonic_core::{AgentId, HarnessEvent, RunId};

    #[test]
    fn sqlite_replay_without_run_reads_latest_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        let mut ledger = SqliteLedger::open_or_create(&path).unwrap();
        let run_1 = RunId::new("run_1").unwrap();
        let run_2 = RunId::new("run_2").unwrap();

        ledger
            .begin_session_run("session_1", &run_1, "first", true)
            .unwrap();
        ledger
            .append(
                "run_1",
                &record(
                    0,
                    HarnessEvent::RunStarted {
                        run_id: run_1.clone(),
                        agent_id: AgentId::new("plato").unwrap(),
                    },
                ),
            )
            .unwrap();
        ledger
            .append(
                "run_1",
                &record(
                    1,
                    HarnessEvent::RunFailed {
                        run_id: run_1.clone(),
                        reason: "synthetic failure".into(),
                    },
                ),
            )
            .unwrap();
        ledger.finish_session_run(&run_1, "first answer").unwrap();
        ledger
            .begin_session_run("session_1", &run_2, "second", false)
            .unwrap();
        ledger
            .append(
                "run_2",
                &record(
                    0,
                    HarnessEvent::RunStarted {
                        run_id: run_2.clone(),
                        agent_id: AgentId::new("plato").unwrap(),
                    },
                ),
            )
            .unwrap();
        ledger
            .append(
                "run_2",
                &record(
                    1,
                    HarnessEvent::RunFailed {
                        run_id: run_2.clone(),
                        reason: "synthetic failure".into(),
                    },
                ),
            )
            .unwrap();
        ledger.finish_session_run(&run_2, "second answer").unwrap();

        let replay = replay_sqlite(&path, None).unwrap();

        assert!(replay.contains("session_id: session_1"));
        assert!(replay.contains("run_id: run_1"));
        assert!(replay.contains("run_id: run_2"));
        assert_eq!(replay.matches("final_phase: Failed").count(), 2);
    }

    fn record(seq: u64, event: HarnessEvent) -> platonic_core::RecordedEvent {
        platonic_core::RecordedEvent {
            seq,
            occurred_at_ms: seq,
            event,
        }
    }
}
