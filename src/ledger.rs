use crate::{AppError, AppResult};
use platonic_core::{HarnessEvent, RecordedEvent, RunId, RunState};
use rusqlite::{Connection, OptionalExtension, params, types::Type};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const LEDGER_VERSION: u32 = 1;

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

impl SqliteLedger {
    pub fn open_or_create(path: &Path) -> AppResult<Self> {
        if path.as_os_str().is_empty() {
            return Err(AppError::EmptyLedger);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        migrate_sqlite(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn open_readonly(path: &Path) -> AppResult<Self> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
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

    #[cfg(test)]
    fn user_version(&self) -> AppResult<u32> {
        let version: u32 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        Ok(version)
    }
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

fn migrate_sqlite(connection: &mut Connection) -> AppResult<()> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            let transaction = connection.transaction()?;
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
                PRAGMA user_version = 1;
                "#,
            )?;
            transaction.commit()?;
            Ok(())
        }
        1 => Ok(()),
        _ => Err(AppError::Config(format!(
            "unsupported sqlite schema version: {version}"
        ))),
    }
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

        assert_eq!(ledger.user_version().unwrap(), 1);
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
