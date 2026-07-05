#![forbid(unsafe_code)]

pub mod app;
pub mod config;
pub mod daemon;
pub mod error;
pub mod ledger;
pub mod model;
pub mod paths;
pub mod replay;
pub mod tool_catalog;
pub mod tools;
pub mod tui;

pub use app::{
    ApprovalMode, ApprovalRequest, RunLedger, RunOptions, RunOutcome, new_run_id, run_question,
};
pub use error::{AppError, AppResult};
pub use replay::{replay_file, replay_sqlite};
