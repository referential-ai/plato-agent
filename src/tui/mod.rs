mod app;
mod modal;
mod render;
mod state;

pub use app::{TuiOptions, run_tui};
pub use modal::{
    ApprovalModalView, approval_from_event, live_event_line, tool_input_preview_from_event,
};
pub use render::{render, render_snapshot};
pub use state::{
    ActiveRunView, ConnectionState, LiveEventLine, TranscriptState, TranscriptView, TuiState,
};
