use crate::{AppError, AppResult};
use platonic_core::{HarnessEvent, RecordedEvent, RunState};
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

pub struct EventRecorder {
    writer: BufWriter<File>,
    state: RunState,
}

impl EventRecorder {
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
        let record = RecordedEvent {
            seq: self.state.next_seq(),
            occurred_at_ms: now_ms(),
            event,
        };
        self.state.apply(&record)?;

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
    fn writes_and_reads_versioned_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut recorder = EventRecorder::create(&path).unwrap();

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
    fn refuses_to_overwrite_existing_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "").unwrap();

        assert!(matches!(
            EventRecorder::create(&path),
            Err(AppError::LedgerExists(_))
        ));
    }
}
