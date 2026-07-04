use crate::{AppResult, ledger};
use platonic_core::{MessageRole, ReadbackEntry, RunReadback};
use std::path::Path;

pub fn replay_file(path: &Path) -> AppResult<String> {
    let records = ledger::read_records(path)?;
    let readback = RunReadback::from_events(&records)?;
    Ok(format_readback(&readback))
}

pub fn replay_sqlite(path: &Path, run_id: Option<&str>) -> AppResult<String> {
    let records = ledger::read_sqlite_records(path, run_id)?;
    let readback = RunReadback::from_events(&records)?;
    Ok(format_readback(&readback))
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
