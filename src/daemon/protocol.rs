use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const PROTOCOL_VERSION: u32 = 1;

pub const ERROR_MALFORMED_REQUEST: &str = "malformed_request";
pub const ERROR_LAGGED: &str = "lagged";
pub const ERROR_INTERNAL: &str = "internal_error";
pub const ERROR_NOT_FOUND: &str = "not_found";
pub const ERROR_OVERLOAD: &str = "overload";
pub const ERROR_RUN_FAILED: &str = "run_failed";
pub const ERROR_SESSIONS_LIST_FAILED: &str = "sessions_list_failed";
pub const ERROR_UNSUPPORTED_METHOD: &str = "unsupported_method";
pub const ERROR_UNSUPPORTED_VERSION: &str = "unsupported_version";
pub const ERROR_WORKSPACE_MISMATCH: &str = "workspace_mismatch";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStateName {
    Running,
    Finished,
    Failed,
    Canceled,
    CancelRequested,
    Interrupted,
}

impl RunStateName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::CancelRequested => "cancel_requested",
            Self::Interrupted => "interrupted",
        }
    }
}

impl fmt::Display for RunStateName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    Request,
    Response,
    Event,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub v: u32,
    pub id: Option<String>,
    pub kind: EnvelopeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
}

impl Envelope {
    pub fn response(id: Option<String>, method: Option<String>, result: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            kind: EnvelopeKind::Response,
            method,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(
        id: Option<String>,
        method: Option<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            kind: EnvelopeKind::Error,
            method,
            params: None,
            result: None,
            error: Some(ProtocolError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    pub workspace_root: String,
    pub workspace_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    pub daemon_version: String,
    pub workspace_id: String,
    pub ledger_path: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStartParams {
    pub question: String,
    #[serde(default)]
    pub config_path: Option<String>,
    #[serde(default)]
    pub wait: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunStartResult {
    pub run_id: String,
    pub session_id: String,
    pub ledger_path: String,
    pub status: RunStateName,
    pub final_answer: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageAppendParams {
    pub message: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub config_path: Option<String>,
    #[serde(default)]
    pub wait: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsStreamParams {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventsStreamResult {
    pub run_id: String,
    pub from_offset: u64,
    pub next_offset: u64,
    pub status: RunStateName,
    pub events: Vec<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecideParams {
    pub run_id: String,
    pub tool_call_id: String,
    pub decision: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelParams {
    pub run_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandAcceptedResult {
    pub run_id: String,
    pub status: RunStateName,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionsListResult {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub run_id: String,
    pub status: RunStateName,
    pub latest_question: String,
    pub ledger_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptReadParams {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TranscriptReadResult {
    pub run_id: String,
    pub status: RunStateName,
    pub final_answer: Option<String>,
    pub transcript: String,
}

pub fn decode_request(line: &str) -> Result<Envelope, Box<Envelope>> {
    let envelope = serde_json::from_str::<Envelope>(line).map_err(|error| {
        Box::new(Envelope::error(
            None,
            None,
            ERROR_MALFORMED_REQUEST,
            format!("request is not a valid protocol envelope: {error}"),
        ))
    })?;

    if envelope.v != PROTOCOL_VERSION {
        return Err(Box::new(Envelope::error(
            envelope.id,
            envelope.method,
            ERROR_UNSUPPORTED_VERSION,
            format!("unsupported protocol version: {}", envelope.v),
        )));
    }
    if envelope.kind != EnvelopeKind::Request {
        return Err(Box::new(Envelope::error(
            envelope.id,
            envelope.method,
            ERROR_MALFORMED_REQUEST,
            "envelope kind must be request",
        )));
    }
    if envelope.method.is_none() {
        return Err(Box::new(Envelope::error(
            envelope.id,
            None,
            ERROR_MALFORMED_REQUEST,
            "request method is required",
        )));
    }

    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_state_names_keep_wire_values() {
        let cases = [
            (RunStateName::Running, "running"),
            (RunStateName::Finished, "finished"),
            (RunStateName::Failed, "failed"),
            (RunStateName::Canceled, "canceled"),
            (RunStateName::CancelRequested, "cancel_requested"),
            (RunStateName::Interrupted, "interrupted"),
        ];

        for (state, wire_value) in cases {
            assert_eq!(state.as_str(), wire_value);
            assert_eq!(serde_json::to_value(state).unwrap(), wire_value);
            assert_eq!(
                serde_json::from_value::<RunStateName>(wire_value.into()).unwrap(),
                state
            );
        }
    }

    #[test]
    fn decodes_request_envelope() {
        let envelope = decode_request(
            r#"{"v":1,"id":"req_1","kind":"request","method":"hello","params":{"workspace_root":"/tmp/work","workspace_id":"work-1234"}}"#,
        )
        .unwrap();

        assert_eq!(envelope.id.as_deref(), Some("req_1"));
        assert_eq!(envelope.method.as_deref(), Some("hello"));
    }

    #[test]
    fn rejects_unsupported_version_with_typed_error() {
        let error =
            decode_request(r#"{"v":2,"id":"req_1","kind":"request","method":"hello","params":{}}"#)
                .unwrap_err();

        assert_eq!(error.kind, EnvelopeKind::Error);
        assert_eq!(
            error.error.unwrap().code,
            ERROR_UNSUPPORTED_VERSION.to_string()
        );
    }

    #[test]
    fn response_serializes_without_request_params() {
        let response = Envelope::response(
            Some("req_1".into()),
            Some("hello".into()),
            serde_json::json!({"workspace_id":"work-1234"}),
        );

        let raw = serde_json::to_string(&response).unwrap();

        assert!(raw.contains(r#""kind":"response""#));
        assert!(!raw.contains("params"));
    }
}
