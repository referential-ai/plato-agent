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
    pub scroll_offset: usize,
    pub active_model: Option<String>,
    pub active_run_elapsed_secs: Option<u64>,
    pub composer: String,
    pub composer_cursor: usize,
    pub queued_messages: Vec<String>,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
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
            scroll_offset: 0,
            active_model: None,
            active_run_elapsed_secs: None,
            composer: String::new(),
            composer_cursor: 0,
            queued_messages: Vec::new(),
            input_history: Vec::new(),
            history_index: None,
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
            scroll_offset: 0,
            active_model: None,
            active_run_elapsed_secs: None,
            composer: String::new(),
            composer_cursor: 0,
            queued_messages: Vec::new(),
            input_history: Vec::new(),
            history_index: None,
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
    pub kind: LiveEventKind,
    pub text: String,
}

impl LiveEventLine {
    pub fn new(offset: Option<u64>, text: impl Into<String>) -> Self {
        Self::status(offset, text)
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            offset: None,
            kind: LiveEventKind::User,
            text: text.into(),
        }
    }

    pub fn assistant(offset: Option<u64>, text: impl Into<String>) -> Self {
        Self {
            offset,
            kind: LiveEventKind::Assistant,
            text: text.into(),
        }
    }

    pub fn assistant_delta(offset: Option<u64>, text: impl Into<String>) -> Self {
        Self {
            offset,
            kind: LiveEventKind::AssistantDelta,
            text: text.into(),
        }
    }

    pub fn tool(offset: Option<u64>, text: impl Into<String>) -> Self {
        Self {
            offset,
            kind: LiveEventKind::Tool,
            text: text.into(),
        }
    }

    pub fn status(offset: Option<u64>, text: impl Into<String>) -> Self {
        Self {
            offset,
            kind: LiveEventKind::Status,
            text: text.into(),
        }
    }

    pub fn warning(offset: Option<u64>, text: impl Into<String>) -> Self {
        Self {
            offset,
            kind: LiveEventKind::Warning,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveEventKind {
    User,
    Assistant,
    AssistantDelta,
    Tool,
    Status,
    Warning,
}
