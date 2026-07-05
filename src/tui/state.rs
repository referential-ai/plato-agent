use crate::daemon::protocol::{HelloResult, SessionSummary, TranscriptReadResult};

use super::ApprovalModalView;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiState {
    pub workspace_root: String,
    pub socket_path: String,
    pub connection: ConnectionState,
    pub sessions: Vec<SessionSummary>,
    pub transcript: TranscriptState,
    pub active_run: Option<ActiveRunView>,
    pub live_events: Vec<LiveEventLine>,
    pub composer: String,
    pub status_message: Option<String>,
    pub stream_warning: Option<String>,
    pub approval: Option<ApprovalModalView>,
    pub cancel_requested: bool,
}

impl TuiState {
    pub fn connected(
        workspace_root: String,
        socket_path: String,
        hello: HelloResult,
        sessions: Vec<SessionSummary>,
        transcript: TranscriptState,
    ) -> Self {
        Self {
            workspace_root,
            socket_path,
            connection: ConnectionState::Connected {
                workspace_id: hello.workspace_id,
                daemon_version: hello.daemon_version,
                ledger_path: hello.ledger_path,
            },
            sessions,
            transcript,
            active_run: None,
            live_events: Vec::new(),
            composer: String::new(),
            status_message: None,
            stream_warning: None,
            approval: None,
            cancel_requested: false,
        }
    }

    pub fn disconnected(workspace_root: String, socket_path: String, error: String) -> Self {
        Self {
            workspace_root,
            socket_path,
            connection: ConnectionState::Disconnected { error },
            sessions: Vec::new(),
            transcript: TranscriptState::None,
            active_run: None,
            live_events: Vec::new(),
            composer: String::new(),
            status_message: None,
            stream_warning: None,
            approval: None,
            cancel_requested: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Connected {
        workspace_id: String,
        daemon_version: String,
        ledger_path: String,
    },
    Disconnected {
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptView {
    pub run_id: String,
    pub content: String,
}

impl From<TranscriptReadResult> for TranscriptView {
    fn from(transcript: TranscriptReadResult) -> Self {
        Self {
            run_id: transcript.run_id,
            content: transcript.transcript,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptState {
    None,
    Loaded(TranscriptView),
    Unavailable { run_id: String, error: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRunView {
    pub run_id: String,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveEventLine {
    pub offset: Option<u64>,
    pub text: String,
}

impl LiveEventLine {
    pub fn new(offset: Option<u64>, text: impl Into<String>) -> Self {
        Self {
            offset,
            text: text.into(),
        }
    }
}
