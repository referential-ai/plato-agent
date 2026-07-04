use std::path::PathBuf;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("provider api key env var {0} is not set")]
    MissingApiKey(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("tool error: {0}")]
    Tool(String),

    #[error("ledger version mismatch: expected {expected}, actual {actual}")]
    LedgerVersion { expected: u32, actual: u32 },

    #[error("ledger path is empty")]
    EmptyLedger,

    #[error("ledger already exists: {0}")]
    LedgerExists(PathBuf),

    #[error("ledger conflict for run {run_id} seq {seq}")]
    LedgerConflict { run_id: String, seq: u64 },

    #[error("sqlite ledger has no runs")]
    NoSqliteRuns,

    #[error("run not found in sqlite ledger: {0}")]
    RunNotFound(String),

    #[error("question is empty")]
    EmptyQuestion,

    #[error("run did not finish: {0}")]
    RunFailed(String),

    #[error("path escapes workspace: {0}")]
    PathEscapesWorkspace(PathBuf),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("core error: {0}")]
    Core(#[from] platonic_core::Error),
}
