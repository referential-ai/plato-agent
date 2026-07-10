use crate::{AppError, AppResult};
use platonic_core::{HarnessEvent, RecordedEvent, RunId, RunState};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const LEDGER_VERSION: u32 = 1;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_SCHEMA_VERSION: u32 = 2;
const RUN_STATUS_RUNNING: &str = "running";
const RUN_STATUS_FINISHED: &str = "finished";
const RUN_STATUS_FAILED: &str = "failed";
const RUN_STATUS_CANCELED: &str = "canceled";
const RUN_STATUS_INTERRUPTED: &str = "interrupted";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerLine {
    pub v: u32,
    pub record: RecordedEvent,
}

pub enum EventRecorder {
    Jsonl(JsonlEventRecorder),
    Sqlite(SqliteEventRecorder),
}

impl EventRecorder {
    pub fn create_jsonl(path: &Path) -> AppResult<Self> {
        Ok(Self::Jsonl(JsonlEventRecorder::create(path)?))
    }

    pub fn create_sqlite(path: &Path, run_id: &RunId) -> AppResult<Self> {
        Ok(Self::Sqlite(SqliteEventRecorder::create(path, run_id)?))
    }

    pub fn record(&mut self, event: HarnessEvent) -> AppResult<RecordedEvent> {
        match self {
            Self::Jsonl(recorder) => recorder.record(event),
            Self::Sqlite(recorder) => recorder.record(event),
        }
    }
}

pub struct JsonlEventRecorder {
    writer: BufWriter<File>,
    state: RunState,
}

impl JsonlEventRecorder {
    pub fn create(path: &Path) -> AppResult<Self> {
        if path.as_os_str().is_empty() {
            return Err(AppError::EmptyLedger);
        }

        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    AppError::LedgerExists(path.into())
                } else {
                    AppError::Io(error)
                }
            })?;
        Ok(Self {
            writer: BufWriter::new(file),
            state: RunState::new(),
        })
    }

    pub fn record(&mut self, event: HarnessEvent) -> AppResult<RecordedEvent> {
        let record = next_record(&mut self.state, event)?;
        let line = LedgerLine {
            v: LEDGER_VERSION,
            record: record.clone(),
        };
        serde_json::to_writer(&mut self.writer, &line)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(record)
    }
}

pub struct SqliteEventRecorder {
    ledger: SqliteLedger,
    run_id: String,
    state: RunState,
}

impl SqliteEventRecorder {
    pub fn create(path: &Path, run_id: &RunId) -> AppResult<Self> {
        Ok(Self {
            ledger: SqliteLedger::open_or_create(path)?,
            run_id: run_id.to_string(),
            state: RunState::new(),
        })
    }

    pub fn record(&mut self, event: HarnessEvent) -> AppResult<RecordedEvent> {
        let record = next_record(&mut self.state, event)?;
        self.ledger.append(&self.run_id, &record)?;
        Ok(record)
    }
}

pub struct SqliteLedger {
    connection: Connection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTurn {
    pub question: String,
    pub final_answer: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRunRecords {
    pub run_id: String,
    pub records: Vec<RecordedEvent>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRecords {
    pub session_id: String,
    pub runs: Vec<SessionRunRecords>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedSessionSummary {
    pub session_id: String,
    pub run_id: String,
    pub status: String,
    pub latest_question: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedRunStatus {
    pub run_id: String,
    pub status: String,
    pub final_answer: Option<String>,
}

impl SqliteLedger {
    pub fn open_or_create(path: &Path) -> AppResult<Self> {
        if path.as_os_str().is_empty() {
            return Err(AppError::EmptyLedger);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        configure_sqlite_connection(&connection)?;
        migrate_sqlite(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn open_readonly(path: &Path) -> AppResult<Self> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        configure_sqlite_connection(&connection)?;
        Ok(Self { connection })
    }

    pub fn append(&mut self, run_id: &str, record: &RecordedEvent) -> AppResult<()> {
        let event_json = serde_json::to_string(&record.event)?;
        let seq = sqlite_i64(record.seq, "seq")?;
        let occurred_at_ms = sqlite_i64(record.occurred_at_ms, "occurred_at_ms")?;
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO ledger_events (run_id, seq, occurred_at_ms, v, event_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, seq, occurred_at_ms, LEDGER_VERSION, event_json],
        )?;
        if inserted == 1 {
            return Ok(());
        }

        let existing = self.connection.query_row(
            "SELECT occurred_at_ms, v, event_json FROM ledger_events WHERE run_id = ?1 AND seq = ?2",
            params![run_id, seq],
            |row| {
                Ok(ExistingEvent {
                    occurred_at_ms: row_u64(row, 0, "occurred_at_ms")?,
                    version: row.get(1)?,
                    event_json: row.get(2)?,
                })
            },
        )?;
        if existing.occurred_at_ms == record.occurred_at_ms
            && existing.version == LEDGER_VERSION
            && existing.event_json == event_json
        {
            Ok(())
        } else {
            Err(AppError::LedgerConflict {
                run_id: run_id.into(),
                seq: record.seq,
            })
        }
    }

    pub fn read_run(&self, run_id: &str) -> AppResult<Vec<RecordedEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT seq, occurred_at_ms, v, event_json
             FROM ledger_events
             WHERE run_id = ?1
             ORDER BY seq ASC",
        )?;
        let records = statement
            .query_map(params![run_id], sqlite_record_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        if records.is_empty() {
            Err(AppError::RunNotFound(run_id.into()))
        } else {
            Ok(records)
        }
    }

    pub fn read_latest_run(&self) -> AppResult<(String, Vec<RecordedEvent>)> {
        let run_id = self
            .connection
            .query_row(
                "SELECT run_id
                 FROM ledger_events
                 GROUP BY run_id
                 ORDER BY MAX(occurred_at_ms) DESC, run_id DESC
                 LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(AppError::NoSqliteRuns)?;
        let records = self.read_run(&run_id)?;
        Ok((run_id, records))
    }

    pub fn latest_session_id(&self) -> AppResult<String> {
        self.connection
            .query_row(
                "SELECT session_id
                 FROM sessions
                 ORDER BY updated_at_ms DESC, session_id DESC
                 LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(AppError::NoSqliteSessions)
    }

    pub fn session_turns(&self, session_id: &str) -> AppResult<Vec<SessionTurn>> {
        if !self.session_exists(session_id)? {
            return Err(AppError::SessionNotFound(session_id.into()));
        }
        let mut statement = self.connection.prepare(
            "SELECT question, final_answer
             FROM session_runs
             WHERE session_id = ?1 AND status = ?2 AND final_answer IS NOT NULL
             ORDER BY session_index ASC",
        )?;
        Ok(statement
            .query_map(params![session_id, RUN_STATUS_FINISHED], |row| {
                Ok(SessionTurn {
                    question: row.get(0)?,
                    final_answer: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn begin_session_run(
        &mut self,
        session_id: &str,
        run_id: &RunId,
        question: &str,
        create_session: bool,
    ) -> AppResult<Vec<SessionTurn>> {
        let now = sqlite_i64(now_ms(), "occurred_at_ms")?;
        let transaction = self.connection.transaction()?;
        let exists = session_exists_in(&transaction, session_id)?;
        if !exists && !create_session {
            return Err(AppError::SessionNotFound(session_id.into()));
        }
        if let Some(active_run_id) = active_run_in(&transaction, session_id)? {
            return Err(AppError::SessionActive {
                session_id: session_id.into(),
                run_id: active_run_id,
            });
        }
        if !exists {
            transaction.execute(
                "INSERT INTO sessions (session_id, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![session_id, now, now],
            )?;
        }
        let session_index: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(session_index) + 1, 0)
             FROM session_runs
             WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO session_runs
               (session_id, run_id, session_index, question, final_answer, status, error, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, NULL, ?6, ?7)",
            params![
                session_id,
                run_id.to_string(),
                session_index,
                question,
                RUN_STATUS_RUNNING,
                now,
                now
            ],
        )?;
        transaction.execute(
            "UPDATE sessions SET updated_at_ms = ?2 WHERE session_id = ?1",
            params![session_id, now],
        )?;
        transaction.commit()?;
        self.session_turns(session_id)
    }

    pub fn finish_session_run(&mut self, run_id: &RunId, final_answer: &str) -> AppResult<()> {
        self.update_session_run(run_id, RUN_STATUS_FINISHED, Some(final_answer), None)
    }

    pub fn fail_session_run(
        &mut self,
        run_id: &RunId,
        error: &str,
        canceled: bool,
    ) -> AppResult<()> {
        let status = if canceled {
            RUN_STATUS_CANCELED
        } else {
            RUN_STATUS_FAILED
        };
        self.update_session_run(run_id, status, None, Some(error))
    }

    pub fn interrupt_running_session_runs(&mut self, error: &str) -> AppResult<usize> {
        let now = sqlite_i64(now_ms(), "occurred_at_ms")?;
        let transaction = self.connection.transaction()?;
        let session_ids = {
            let mut statement = transaction.prepare(
                "SELECT DISTINCT session_id
                 FROM session_runs
                 WHERE status = ?1",
            )?;
            statement
                .query_map(params![RUN_STATUS_RUNNING], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let updated = transaction.execute(
            "UPDATE session_runs
             SET status = ?2, error = ?3, updated_at_ms = ?4
             WHERE status = ?1",
            params![RUN_STATUS_RUNNING, RUN_STATUS_INTERRUPTED, error, now],
        )?;
        for session_id in session_ids {
            transaction.execute(
                "UPDATE sessions SET updated_at_ms = ?2 WHERE session_id = ?1",
                params![session_id, now],
            )?;
        }
        transaction.commit()?;
        Ok(updated)
    }

    pub fn read_session(&self, session_id: &str) -> AppResult<SessionRecords> {
        if !self.session_exists(session_id)? {
            return Err(AppError::SessionNotFound(session_id.into()));
        }
        let mut statement = self.connection.prepare(
            "SELECT run_id
             FROM session_runs
             WHERE session_id = ?1
             ORDER BY session_index ASC",
        )?;
        let run_ids = statement
            .query_map(params![session_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let runs = run_ids
            .into_iter()
            .map(|run_id| {
                Ok(SessionRunRecords {
                    records: self.read_run(&run_id)?,
                    run_id,
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        Ok(SessionRecords {
            session_id: session_id.into(),
            runs,
        })
    }

    pub fn read_latest_session(&self) -> AppResult<SessionRecords> {
        let session_id = self.latest_session_id()?;
        self.read_session(&session_id)
    }

    pub fn session_summaries(&self) -> AppResult<Vec<PersistedSessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT s.session_id, sr.run_id, sr.status, sr.question
             FROM sessions s
             JOIN session_runs sr ON sr.session_id = s.session_id
             WHERE sr.session_index = (
               SELECT MAX(session_index)
               FROM session_runs
               WHERE session_id = s.session_id
             )
             ORDER BY s.updated_at_ms DESC, s.session_id DESC",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(PersistedSessionSummary {
                    session_id: row.get(0)?,
                    run_id: row.get(1)?,
                    status: row.get(2)?,
                    latest_question: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub(crate) fn run_status(&self, run_id: &str) -> AppResult<PersistedRunStatus> {
        self.connection
            .query_row(
                "SELECT run_id, status, final_answer FROM session_runs WHERE run_id = ?1",
                params![run_id],
                persisted_run_status,
            )
            .optional()?
            .ok_or_else(|| AppError::RunNotFound(run_id.into()))
    }

    pub(crate) fn latest_session_run_status(
        &self,
        session_id: &str,
    ) -> AppResult<PersistedRunStatus> {
        self.connection
            .query_row(
                "SELECT run_id, status, final_answer
                 FROM session_runs
                 WHERE session_id = ?1
                 ORDER BY session_index DESC
                 LIMIT 1",
                params![session_id],
                persisted_run_status,
            )
            .optional()?
            .ok_or_else(|| AppError::SessionNotFound(session_id.into()))
    }

    fn session_exists(&self, session_id: &str) -> AppResult<bool> {
        session_exists_in(&self.connection, session_id)
    }

    fn update_session_run(
        &mut self,
        run_id: &RunId,
        status: &str,
        final_answer: Option<&str>,
        error: Option<&str>,
    ) -> AppResult<()> {
        let now = sqlite_i64(now_ms(), "occurred_at_ms")?;
        let transaction = self.connection.transaction()?;
        let updated = transaction.execute(
            "UPDATE session_runs
             SET status = ?2, final_answer = ?3, error = ?4, updated_at_ms = ?5
             WHERE run_id = ?1",
            params![run_id.to_string(), status, final_answer, error, now],
        )?;
        if updated == 0 {
            return Err(AppError::RunNotFound(run_id.to_string()));
        }
        let session_id: String = transaction.query_row(
            "SELECT session_id FROM session_runs WHERE run_id = ?1",
            params![run_id.to_string()],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE sessions SET updated_at_ms = ?2 WHERE session_id = ?1",
            params![session_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(test)]
    fn user_version(&self) -> AppResult<u32> {
        let version: u32 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        Ok(version)
    }
}

fn persisted_run_status(row: &rusqlite::Row<'_>) -> rusqlite::Result<PersistedRunStatus> {
    Ok(PersistedRunStatus {
        run_id: row.get(0)?,
        status: row.get(1)?,
        final_answer: row.get(2)?,
    })
}

struct ExistingEvent {
    occurred_at_ms: u64,
    version: u32,
    event_json: String,
}

fn sqlite_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecordedEvent> {
    let version: u32 = row.get(2)?;
    if version != LEDGER_VERSION {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let event_json: String = row.get(3)?;
    let event = serde_json::from_str(&event_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(error))
    })?;
    Ok(RecordedEvent {
        seq: row_u64(row, 0, "seq")?,
        occurred_at_ms: row_u64(row, 1, "occurred_at_ms")?,
        event,
    })
}

fn sqlite_i64(value: u64, field: &str) -> AppResult<i64> {
    value
        .try_into()
        .map_err(|_| AppError::Config(format!("ledger {field} exceeds sqlite integer: {value}")))
}

fn row_u64(row: &rusqlite::Row<'_>, index: usize, field: &str) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    value.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("ledger {field} is negative: {value}"),
            )),
        )
    })
}

fn session_exists_in(connection: &Connection, session_id: &str) -> AppResult<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sessions WHERE session_id = ?1 LIMIT 1",
            params![session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn active_run_in(connection: &Connection, session_id: &str) -> AppResult<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT run_id
             FROM session_runs
             WHERE session_id = ?1 AND status = ?2
             ORDER BY session_index ASC
             LIMIT 1",
            params![session_id, RUN_STATUS_RUNNING],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

fn configure_sqlite_connection(connection: &Connection) -> AppResult<()> {
    connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    Ok(())
}

fn migrate_sqlite(connection: &mut Connection) -> AppResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            transaction.execute_batch(
                r#"
                CREATE TABLE ledger_events (
                  run_id TEXT NOT NULL,
                  seq INTEGER NOT NULL,
                  occurred_at_ms INTEGER NOT NULL,
                  v INTEGER NOT NULL,
                  event_json TEXT NOT NULL,
                  PRIMARY KEY (run_id, seq)
                );
                "#,
            )?;
            create_session_tables(&transaction)?;
            transaction.pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION)?;
        }
        1 => {
            create_session_tables(&transaction)?;
            transaction.pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION)?;
        }
        SQLITE_SCHEMA_VERSION => {}
        _ => {
            return Err(AppError::Config(format!(
                "unsupported sqlite schema version: {version}"
            )));
        }
    }
    transaction.commit()?;
    Ok(())
}

fn create_session_tables(connection: &Connection) -> AppResult<()> {
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
          session_id TEXT PRIMARY KEY,
          created_at_ms INTEGER NOT NULL,
          updated_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS session_runs (
          session_id TEXT NOT NULL,
          run_id TEXT PRIMARY KEY,
          session_index INTEGER NOT NULL,
          question TEXT NOT NULL,
          final_answer TEXT,
          status TEXT NOT NULL,
          error TEXT,
          created_at_ms INTEGER NOT NULL,
          updated_at_ms INTEGER NOT NULL,
          UNIQUE(session_id, session_index)
        );

        CREATE INDEX IF NOT EXISTS session_runs_session_index
          ON session_runs(session_id, session_index);
        "#,
    )?;
    Ok(())
}

fn next_record(state: &mut RunState, event: HarnessEvent) -> AppResult<RecordedEvent> {
    let record = RecordedEvent {
        seq: state.next_seq(),
        occurred_at_ms: now_ms(),
        event,
    };
    state.apply(&record)?;
    Ok(record)
}

pub fn read_records(path: &Path) -> AppResult<Vec<RecordedEvent>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let line: LedgerLine = serde_json::from_str(&line)?;
        if line.v != LEDGER_VERSION {
            return Err(AppError::LedgerVersion {
                expected: LEDGER_VERSION,
                actual: line.v,
            });
        }
        records.push(line.record);
    }

    Ok(records)
}

pub fn read_sqlite_records(path: &Path, run_id: Option<&str>) -> AppResult<Vec<RecordedEvent>> {
    let ledger = SqliteLedger::open_readonly(path)?;
    match run_id {
        Some(run_id) => ledger.read_run(run_id),
        None => ledger.read_latest_run().map(|(_, records)| records),
    }
}

pub fn latest_sqlite_session_id(path: &Path) -> AppResult<String> {
    SqliteLedger::open_or_create(path)?.latest_session_id()
}

pub fn read_latest_sqlite_session(path: &Path) -> AppResult<SessionRecords> {
    SqliteLedger::open_readonly(path)?.read_latest_session()
}

pub fn read_sqlite_session(path: &Path, session_id: &str) -> AppResult<SessionRecords> {
    SqliteLedger::open_readonly(path)?.read_session(session_id)
}

pub fn sqlite_session_summaries(path: &Path) -> AppResult<Vec<PersistedSessionSummary>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    SqliteLedger::open_readonly(path)?.session_summaries()
}

pub fn interrupt_orphaned_sqlite_runs(path: &Path) -> AppResult<usize> {
    if !path.exists() {
        return Ok(0);
    }
    SqliteLedger::open_or_create(path)?
        .interrupt_running_session_runs("daemon restarted before run completed")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use platonic_core::{AgentId, HarnessEvent, RunId};

    #[test]
    fn writes_and_reads_versioned_jsonl_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut recorder = JsonlEventRecorder::create(&path).unwrap();

        recorder
            .record(HarnessEvent::RunStarted {
                run_id: RunId::new("run_1").unwrap(),
                agent_id: AgentId::new("plato").unwrap(),
            })
            .unwrap();

        let records = read_records(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq, 0);
    }

    #[test]
    fn rejects_wrong_ledger_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, r#"{"v":2,"record":{"seq":0,"occurred_at_ms":0,"event":{"event":"run_started","run_id":"run_1","agent_id":"plato"}}}"#).unwrap();

        assert!(matches!(
            read_records(&path),
            Err(AppError::LedgerVersion {
                expected: LEDGER_VERSION,
                actual: 2
            })
        ));
    }

    #[test]
    fn refuses_to_overwrite_existing_jsonl_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "").unwrap();

        assert!(matches!(
            JsonlEventRecorder::create(&path),
            Err(AppError::LedgerExists(_))
        ));
    }

    #[test]
    fn migrates_empty_sqlite_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let ledger = SqliteLedger::open_or_create(&path).unwrap();

        assert_eq!(ledger.user_version().unwrap(), SQLITE_SCHEMA_VERSION);
    }

    #[test]
    fn sqlite_append_is_idempotent_and_conflict_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let mut ledger = SqliteLedger::open_or_create(&path).unwrap();
        let record = started_record("run_1", 0, 10);

        ledger.append("run_1", &record).unwrap();
        ledger.append("run_1", &record).unwrap();

        let mut changed = record.clone();
        changed.occurred_at_ms = 11;
        assert!(matches!(
            ledger.append("run_1", &changed),
            Err(AppError::LedgerConflict { .. })
        ));
    }

    #[test]
    fn sqlite_reads_latest_run_when_run_is_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let mut ledger = SqliteLedger::open_or_create(&path).unwrap();
        ledger
            .append("run_old", &started_record("run_old", 0, 10))
            .unwrap();
        ledger
            .append("run_new", &started_record("run_new", 0, 20))
            .unwrap();

        let (run_id, records) = ledger.read_latest_run().unwrap();

        assert_eq!(run_id, "run_new");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn sqlite_sessions_track_finished_turns_and_latest_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let mut ledger = SqliteLedger::open_or_create(&path).unwrap();
        let run_id = RunId::new("run_1").unwrap();

        let turns = ledger
            .begin_session_run("session_1", &run_id, "hello", true)
            .unwrap();
        assert!(turns.is_empty());
        ledger.finish_session_run(&run_id, "hi").unwrap();

        assert_eq!(ledger.latest_session_id().unwrap(), "session_1");
        assert_eq!(
            ledger.session_turns("session_1").unwrap(),
            vec![SessionTurn {
                question: "hello".into(),
                final_answer: "hi".into(),
            }]
        );
    }

    #[test]
    fn sqlite_session_summaries_report_latest_session_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let mut ledger = SqliteLedger::open_or_create(&path).unwrap();
        let first_run = RunId::new("run_1").unwrap();
        let second_run = RunId::new("run_2").unwrap();

        ledger
            .begin_session_run("session_1", &first_run, "first question", true)
            .unwrap();
        ledger
            .finish_session_run(&first_run, "first answer")
            .unwrap();
        ledger
            .begin_session_run("session_1", &second_run, "second question", false)
            .unwrap();

        assert_eq!(
            ledger.session_summaries().unwrap(),
            vec![PersistedSessionSummary {
                session_id: "session_1".into(),
                run_id: "run_2".into(),
                status: RUN_STATUS_RUNNING.into(),
                latest_question: "second question".into(),
            }]
        );
    }

    #[test]
    fn sqlite_interrupts_running_session_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let mut ledger = SqliteLedger::open_or_create(&path).unwrap();
        let run_id = RunId::new("run_1").unwrap();

        ledger
            .begin_session_run("session_1", &run_id, "first question", true)
            .unwrap();

        assert_eq!(
            ledger
                .interrupt_running_session_runs("daemon restarted")
                .unwrap(),
            1
        );
        assert_eq!(
            ledger.session_summaries().unwrap()[0].status,
            RUN_STATUS_INTERRUPTED
        );
        ledger
            .begin_session_run(
                "session_1",
                &RunId::new("run_2").unwrap(),
                "follow up",
                false,
            )
            .unwrap();
    }

    #[test]
    fn sqlite_session_begin_rejects_active_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let mut ledger = SqliteLedger::open_or_create(&path).unwrap();

        ledger
            .begin_session_run(
                "session_1",
                &RunId::new("run_active").unwrap(),
                "hello",
                true,
            )
            .unwrap();
        let error = ledger
            .begin_session_run(
                "session_1",
                &RunId::new("run_next").unwrap(),
                "again",
                false,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            AppError::SessionActive {
                session_id,
                run_id
            } if session_id == "session_1" && run_id == "run_active"
        ));
    }

    #[test]
    fn jsonl_and_sqlite_reconstruct_same_record() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl_path = dir.path().join("events.jsonl");
        let sqlite_path = dir.path().join("events.db");
        let run_id = RunId::new("run_1").unwrap();
        let mut jsonl = JsonlEventRecorder::create(&jsonl_path).unwrap();
        let mut sqlite = SqliteEventRecorder::create(&sqlite_path, &run_id).unwrap();
        let event = HarnessEvent::RunStarted {
            run_id,
            agent_id: AgentId::new("plato").unwrap(),
        };

        let jsonl_record = jsonl.record(event.clone()).unwrap();
        let sqlite_record = sqlite.record(event).unwrap();

        assert_eq!(jsonl_record.seq, sqlite_record.seq);
        assert_eq!(
            read_records(&jsonl_path).unwrap()[0].event,
            sqlite_record.event
        );
        assert_eq!(
            read_sqlite_records(&sqlite_path, Some("run_1")).unwrap()[0].event,
            jsonl_record.event
        );
    }

    fn started_record(run_id: &str, seq: u64, occurred_at_ms: u64) -> RecordedEvent {
        RecordedEvent {
            seq,
            occurred_at_ms,
            event: HarnessEvent::RunStarted {
                run_id: RunId::new(run_id).unwrap(),
                agent_id: AgentId::new("plato").unwrap(),
            },
        }
    }
}
