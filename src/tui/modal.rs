use serde_json::Value;

use super::LiveEventLine;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalModalView {
    pub run_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub effect: String,
    pub reason: String,
    pub input_preview: String,
}

pub fn approval_from_event(
    value: &Value,
    input_preview: Option<String>,
) -> Option<ApprovalModalView> {
    let event = value.get("event").unwrap_or(value);
    if event.get("kind").and_then(Value::as_str) != Some("approval_requested") {
        return None;
    }
    Some(ApprovalModalView {
        run_id: event.get("run_id")?.as_str()?.into(),
        tool_call_id: event.get("tool_call_id")?.as_str()?.into(),
        tool_name: event.get("tool_name")?.as_str()?.into(),
        effect: event
            .get("effect")
            .and_then(Value::as_str)
            .unwrap_or("unknown effect")
            .into(),
        reason: event
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("approval required")
            .into(),
        input_preview: input_preview.unwrap_or_else(|| "input preview unavailable".into()),
    })
}

pub fn tool_input_preview_from_event(value: &Value) -> Option<(String, String)> {
    let event = value.get("event").unwrap_or(value);
    if event.get("kind").and_then(Value::as_str) != Some("ledger")
        || event.pointer("/record/event/event").and_then(Value::as_str)
            != Some("tool_call_proposed")
    {
        return None;
    }
    let call_id = event
        .pointer("/record/event/call/id")?
        .as_str()?
        .to_string();
    let input = event.pointer("/record/event/call/input")?;
    let preview =
        serde_json::to_string_pretty(input).unwrap_or_else(|_| "input preview unavailable".into());
    Some((call_id, truncate_preview(preview, 1200)))
}

pub fn live_event_line(value: &Value) -> LiveEventLine {
    let offset = value.get("offset").and_then(Value::as_u64);
    let event = value.get("event").unwrap_or(value);
    let text = match event.get("kind").and_then(Value::as_str) {
        Some("ledger") => ledger_event_line(event),
        Some("approval_requested") => {
            let tool_name = event
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown tool");
            let effect = event
                .get("effect")
                .and_then(Value::as_str)
                .unwrap_or("unknown effect");
            format!("approval pending {tool_name} ({effect})")
        }
        Some(kind) => kind.into(),
        None => serde_json::to_string(event).unwrap_or_else(|_| "unrenderable event".into()),
    };
    LiveEventLine::new(offset, text)
}

fn ledger_event_line(event: &Value) -> String {
    let event_name = event
        .pointer("/record/event/event")
        .and_then(Value::as_str)
        .unwrap_or("ledger event");
    match event_name {
        "model_responded" => "assistant response".into(),
        "tool_call_proposed" => {
            let tool = event
                .pointer("/record/event/call/tool")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            format!("tool proposed {tool}")
        }
        "tool_finished" => "tool finished".into(),
        "run_finished" => "run finished".into(),
        "run_failed" => "run failed".into(),
        other => other.replace('_', " "),
    }
}

fn truncate_preview(mut preview: String, max_chars: usize) -> String {
    if preview.chars().count() <= max_chars {
        return preview;
    }
    preview = preview.chars().take(max_chars).collect();
    preview.push_str("\n... truncated");
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_daemon_event_lines() {
        let approval = live_event_line(&serde_json::json!({
            "offset": 4,
            "event": {
                "kind": "approval_requested",
                "tool_name": "file.write",
                "effect": "WorkspaceWrite"
            }
        }));
        let ledger = live_event_line(&serde_json::json!({
            "offset": 5,
            "event": {
                "kind": "ledger",
                "record": {
                    "event": {
                        "event": "tool_call_proposed",
                        "call": {
                            "tool": "file.read"
                        }
                    }
                }
            }
        }));

        assert_eq!(
            approval,
            LiveEventLine::new(Some(4), "approval pending file.write (WorkspaceWrite)")
        );
        assert_eq!(
            ledger,
            LiveEventLine::new(Some(5), "tool proposed file.read")
        );
    }

    #[test]
    fn extracts_tool_input_preview_and_approval_modal_from_events() {
        let proposed = serde_json::json!({
            "offset": 3,
            "event": {
                "kind": "ledger",
                "record": {
                    "event": {
                        "event": "tool_call_proposed",
                        "call": {
                            "id": "call_1",
                            "tool": "file.write",
                            "effect": "WorkspaceWrite",
                            "input": {
                                "path": "scratch.txt",
                                "content": "hello"
                            }
                        }
                    }
                }
            }
        });
        let approval = serde_json::json!({
            "offset": 4,
            "event": {
                "kind": "approval_requested",
                "run_id": "run_1",
                "tool_call_id": "call_1",
                "tool_name": "file.write",
                "effect": "WorkspaceWrite",
                "reason": "file.write requires approval"
            }
        });
        let (call_id, input_preview) = tool_input_preview_from_event(&proposed).unwrap();
        let modal = approval_from_event(&approval, Some(input_preview)).unwrap();

        assert_eq!(call_id, "call_1");
        assert_eq!(modal.run_id, "run_1");
        assert!(modal.input_preview.contains("scratch.txt"));
        assert!(modal.input_preview.contains("hello"));
    }
}
