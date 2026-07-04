use crate::{AppError, AppResult};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn default_sqlite_path(workspace_root: &Path) -> AppResult<PathBuf> {
    Ok(state_home()?
        .join("plato-agent")
        .join("workspaces")
        .join(workspace_id(workspace_root)?)
        .join("agent.db"))
}

pub fn default_socket_path(workspace_root: &Path) -> AppResult<PathBuf> {
    Ok(runtime_home()?
        .join("plato-agent")
        .join("workspaces")
        .join(workspace_id(workspace_root)?)
        .join("agent.sock"))
}

pub fn default_lock_path(workspace_root: &Path) -> AppResult<PathBuf> {
    Ok(runtime_home()?
        .join("plato-agent")
        .join("workspaces")
        .join(workspace_id(workspace_root)?)
        .join("agent.lock"))
}

pub fn workspace_id(workspace_root: &Path) -> AppResult<String> {
    let canonical = workspace_root.canonicalize()?;
    Ok(workspace_id_from_canonical_path(&canonical))
}

fn workspace_id_from_canonical_path(path: &Path) -> String {
    let basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    format!("{}-{}", slug(basename), hash16(path))
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut last_was_dash = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            last_was_dash = false;
        } else if !last_was_dash && !output.is_empty() {
            output.push('-');
            last_was_dash = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "workspace".into()
    } else {
        output
    }
}

fn hash16(path: &Path) -> String {
    let digest = Sha256::digest(path_bytes(path));
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().to_string_lossy().as_bytes().to_vec()
}

fn state_home() -> AppResult<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AppError::Config("HOME is required for default --db path".into()))?;
    Ok(PathBuf::from(home).join(".local").join("state"))
}

fn runtime_home() -> AppResult<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_RUNTIME_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    Ok(std::env::temp_dir().join("plato-agent").join(user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_uses_slug_and_hash16() {
        let id = workspace_id_from_canonical_path(Path::new("/tmp/Platonic Workspace"));

        assert!(id.starts_with("platonic-workspace-"));
        assert_eq!(id.rsplit_once('-').unwrap().1.len(), 16);
    }

    #[test]
    fn default_sqlite_path_uses_workspace_directory() {
        let dir = tempfile::tempdir().unwrap();

        let path = default_sqlite_path(dir.path()).unwrap();

        assert!(
            path.components()
                .any(|component| component.as_os_str() == "plato-agent")
        );
        assert!(
            path.components()
                .any(|component| component.as_os_str() == "workspaces")
        );
        assert_eq!(path.file_name().unwrap(), "agent.db");
    }

    #[test]
    fn default_socket_and_lock_paths_use_workspace_directory() {
        let dir = tempfile::tempdir().unwrap();

        let socket_path = default_socket_path(dir.path()).unwrap();
        let lock_path = default_lock_path(dir.path()).unwrap();

        assert!(
            socket_path
                .components()
                .any(|component| component.as_os_str() == "plato-agent")
        );
        assert!(
            socket_path
                .components()
                .any(|component| component.as_os_str() == "workspaces")
        );
        assert_eq!(socket_path.file_name().unwrap(), "agent.sock");
        assert_eq!(lock_path.file_name().unwrap(), "agent.lock");
        assert_eq!(socket_path.parent(), lock_path.parent());
    }
}
