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

#[cfg(unix)]
pub fn default_socket_path(workspace_root: &Path) -> AppResult<PathBuf> {
    Ok(runtime_home()?
        .join("plato-agent")
        .join("workspaces")
        .join(workspace_id(workspace_root)?)
        .join("agent.sock"))
}

#[cfg(windows)]
pub fn default_socket_path(workspace_root: &Path) -> AppResult<PathBuf> {
    Ok(PathBuf::from(format!(
        r"\\.\pipe\plato-agent-{}",
        workspace_id(workspace_root)?
    )))
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
    let slug = slug(basename);
    #[cfg(windows)]
    let slug: String = slug.chars().take(64).collect();
    format!("{slug}-{}", hash16(path))
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

#[cfg(windows)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(unix)]
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

#[cfg(windows)]
fn state_home() -> AppResult<PathBuf> {
    local_app_data("default --db path")
}

#[cfg(unix)]
pub(crate) fn runtime_home() -> AppResult<PathBuf> {
    Ok(runtime_home_and_fallback().0)
}

#[cfg(unix)]
pub(crate) fn runtime_home_and_fallback() -> (PathBuf, bool) {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(value) => (PathBuf::from(value), false),
        None => (
            std::env::temp_dir().join(format!(
                "plato-agent-{}",
                rustix::process::geteuid().as_raw()
            )),
            true,
        ),
    }
}

#[cfg(windows)]
pub(crate) fn runtime_home() -> AppResult<PathBuf> {
    local_app_data("default daemon runtime path")
}

#[cfg(windows)]
fn local_app_data(purpose: &str) -> AppResult<PathBuf> {
    let value = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::Config(format!("LOCALAPPDATA is required for {purpose}")))?;
    Ok(PathBuf::from(value))
}

#[cfg(test)]
pub(crate) fn with_test_xdg<T>(root: &Path, run: impl FnOnce() -> T) -> T {
    #[cfg(unix)]
    {
        let state_home = root.join("xdg-state");
        let runtime_home = root.join("xdg-runtime");
        temp_env::with_vars(
            [
                ("XDG_STATE_HOME", Some(state_home.as_os_str())),
                ("XDG_RUNTIME_DIR", Some(runtime_home.as_os_str())),
            ],
            run,
        )
    }
    #[cfg(windows)]
    {
        let local_app_data = root.join("local-app-data");
        temp_env::with_var("LOCALAPPDATA", Some(local_app_data.as_os_str()), run)
    }
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
        with_test_xdg(dir.path(), || {
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
        });
    }

    #[cfg(unix)]
    #[test]
    fn default_socket_and_lock_paths_use_workspace_directory() {
        let dir = tempfile::tempdir().unwrap();
        with_test_xdg(dir.path(), || {
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
        });
    }

    #[cfg(unix)]
    #[test]
    fn fallback_runtime_home_uses_numeric_uid_under_temp_root() {
        let root = tempfile::tempdir().unwrap();
        temp_env::with_vars(
            [
                ("XDG_RUNTIME_DIR", None),
                ("TMPDIR", Some(root.path().as_os_str())),
                ("USER", Some(std::ffi::OsStr::new("spoofed-name"))),
            ],
            || {
                assert_eq!(
                    runtime_home().unwrap(),
                    root.path().join(format!(
                        "plato-agent-{}",
                        rustix::process::geteuid().as_raw()
                    ))
                );
            },
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_use_local_app_data_and_workspace_pipe() {
        let workspace = tempfile::tempdir().unwrap();
        let local_app_data = tempfile::tempdir().unwrap();
        temp_env::with_var(
            "LOCALAPPDATA",
            Some(local_app_data.path().as_os_str()),
            || {
                let workspace_id = workspace_id(workspace.path()).unwrap();
                let workspace_dir = local_app_data
                    .path()
                    .join("plato-agent")
                    .join("workspaces")
                    .join(&workspace_id);

                assert_eq!(
                    default_socket_path(workspace.path()).unwrap(),
                    PathBuf::from(format!(r"\\.\pipe\plato-agent-{workspace_id}"))
                );
                assert_eq!(
                    default_lock_path(workspace.path()).unwrap(),
                    workspace_dir.join("agent.lock")
                );
                assert_eq!(
                    default_sqlite_path(workspace.path()).unwrap(),
                    workspace_dir.join("agent.db")
                );
            },
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_endpoint_is_bounded_for_long_workspace_names() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace-".repeat(20));
        std::fs::create_dir(&workspace).unwrap();

        let endpoint = default_socket_path(&workspace).unwrap();
        let endpoint = endpoint.to_string_lossy();
        let workspace_id = workspace_id(&workspace).unwrap();

        assert_eq!(endpoint, format!(r"\\.\pipe\plato-agent-{workspace_id}"));
        assert!(endpoint.encode_utf16().count() <= 102);
        assert!(workspace_id.len() <= 81);
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_require_local_app_data() {
        let workspace = tempfile::tempdir().unwrap();
        temp_env::with_var_unset("LOCALAPPDATA", || {
            let error = default_lock_path(workspace.path()).unwrap_err();

            assert!(error.to_string().contains("LOCALAPPDATA"));
        });
    }
}
