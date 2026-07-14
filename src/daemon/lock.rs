use crate::{AppError, AppResult, paths};
use serde::{Deserialize, Serialize};
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

const LOCK_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockMetadata {
    pub v: u32,
    pub pid: u32,
    pub executable: Option<String>,
    pub workspace_root: String,
    pub workspace_id: String,
    pub socket_path: String,
}

impl LockMetadata {
    pub fn for_workspace(workspace_root: &Path, socket_path: &Path) -> AppResult<Self> {
        Ok(Self {
            v: LOCK_VERSION,
            pid: std::process::id(),
            executable: std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
            workspace_root: workspace_root
                .canonicalize()?
                .to_string_lossy()
                .into_owned(),
            workspace_id: paths::workspace_id(workspace_root)?,
            socket_path: socket_path.to_string_lossy().into_owned(),
        })
    }

    fn summary(&self) -> String {
        let executable = self.executable.as_deref().unwrap_or("unknown executable");
        format!(
            "pid={}, executable={}, workspace_id={}, socket_path={}",
            self.pid, executable, self.workspace_id, self.socket_path
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockConflict {
    pub path: PathBuf,
    pub metadata: Option<LockMetadata>,
    pub metadata_error: Option<String>,
}

impl LockConflict {
    pub fn owner_summary(&self) -> String {
        if let Some(metadata) = &self.metadata {
            metadata.summary()
        } else if let Some(error) = &self.metadata_error {
            format!("metadata unreadable: {error}")
        } else {
            "metadata missing".into()
        }
    }
}

#[derive(Debug)]
pub struct WorkspaceLock {
    path: PathBuf,
}

impl WorkspaceLock {
    pub fn acquire(path: PathBuf, metadata: LockMetadata) -> Result<Self, Box<LockConflict>> {
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return Err(Box::new(LockConflict {
                path,
                metadata: None,
                metadata_error: Some(error.to_string()),
            }));
        }

        let mut file = match create_lock_file(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Err(Box::new(read_conflict(path)));
            }
            Err(error) => {
                return Err(Box::new(LockConflict {
                    path,
                    metadata: None,
                    metadata_error: Some(error.to_string()),
                }));
            }
        };

        if let Err(error) = serde_json::to_writer(&mut file, &metadata)
            .and_then(|()| file.write_all(b"\n").map_err(serde_json::Error::io))
        {
            let _ = fs::remove_file(&path);
            return Err(Box::new(LockConflict {
                path,
                metadata: None,
                metadata_error: Some(error.to_string()),
            }));
        }

        Ok(Self { path })
    }

    pub fn acquire_for_workspace(workspace_root: &Path, socket_path: &Path) -> AppResult<Self> {
        let lock_path = paths::default_lock_path(workspace_root)?;
        let metadata = LockMetadata::for_workspace(workspace_root, socket_path)?;
        Self::acquire(lock_path, metadata).map_err(|conflict| lock_conflict_error(*conflict))
    }
}

#[cfg(not(windows))]
fn create_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(windows)]
fn create_lock_file(path: &Path) -> std::io::Result<File> {
    crate::windows_security::create_current_user_file(path)
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn ensure_workspace_unlocked(workspace_root: &Path) -> AppResult<()> {
    let lock_path = paths::default_lock_path(workspace_root)?;
    if lock_path.exists() {
        return Err(lock_conflict_error(read_conflict(lock_path)));
    }
    Ok(())
}

fn read_conflict(path: PathBuf) -> LockConflict {
    match fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<LockMetadata>(raw.trim()) {
            Ok(metadata) => LockConflict {
                path,
                metadata: Some(metadata),
                metadata_error: None,
            },
            Err(error) => LockConflict {
                path,
                metadata: None,
                metadata_error: Some(error.to_string()),
            },
        },
        Err(error) => LockConflict {
            path,
            metadata: None,
            metadata_error: Some(error.to_string()),
        },
    }
}

fn lock_conflict_error(conflict: LockConflict) -> AppError {
    let owner = conflict.owner_summary();
    AppError::DaemonLockHeld {
        path: conflict.path,
        owner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_conflict_reports_owner_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("agent.lock");
        let workspace = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("agent.sock");
        let metadata = LockMetadata::for_workspace(workspace.path(), &socket_path).unwrap();
        let _lock = WorkspaceLock::acquire(lock_path.clone(), metadata.clone()).unwrap();

        let conflict =
            WorkspaceLock::acquire(lock_path, metadata).expect_err("second lock must conflict");

        assert!(conflict.owner_summary().contains("pid="));
        assert!(conflict.owner_summary().contains("workspace_id="));
        assert!(conflict.owner_summary().contains("socket_path="));
    }

    #[test]
    fn dropping_lock_removes_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("agent.lock");
        let workspace = tempfile::tempdir().unwrap();
        let metadata =
            LockMetadata::for_workspace(workspace.path(), &dir.path().join("agent.sock")).unwrap();

        {
            let _lock = WorkspaceLock::acquire(lock_path.clone(), metadata).unwrap();
            assert!(lock_path.exists());
        }

        assert!(!lock_path.exists());
    }

    #[test]
    fn lock_conflict_reports_unreadable_metadata_without_stealing() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("agent.lock");
        std::fs::write(&lock_path, "not json").unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let metadata =
            LockMetadata::for_workspace(workspace.path(), &dir.path().join("agent.sock")).unwrap();

        let conflict = WorkspaceLock::acquire(lock_path.clone(), metadata)
            .expect_err("corrupt existing lock still conflicts");

        assert!(conflict.owner_summary().contains("metadata unreadable"));
        assert_eq!(std::fs::read_to_string(lock_path).unwrap(), "not json");
    }
}
