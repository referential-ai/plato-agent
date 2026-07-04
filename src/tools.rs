use crate::tool_catalog::{FILE_LIST, FILE_READ, FILE_WRITE};
use crate::{AppError, AppResult};
use platonic_core::{ResultVisibility, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_LIST_ENTRIES: usize = 200;
const MAX_LIST_DATA_BYTES: usize = 32 * 1024;
const APPROVAL_PREVIEW_CHARS: usize = 1_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileReadInput {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileListInput {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileWriteInput {
    path: String,
    content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalOutcome {
    Granted,
    Denied { reason: String },
}

pub fn execute_tool(
    workspace_root: &Path,
    call_id: platonic_core::ToolCallId,
    tool_name: &str,
    input: Value,
) -> AppResult<ToolResult> {
    match tool_name {
        FILE_READ => read_file(workspace_root, call_id, input),
        FILE_LIST => list_directory(workspace_root, call_id, input),
        FILE_WRITE => write_file(workspace_root, call_id, input),
        _ => Err(AppError::Tool(format!("unknown tool: {tool_name}"))),
    }
}

pub fn ask_for_approval(tool_name: &str, input: &Value) -> AppResult<ApprovalOutcome> {
    let preview = approval_preview(input);
    eprint!("Approve {tool_name} {preview}? [y/N] ");
    io::stderr().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let normalized = line.trim().to_ascii_lowercase();
    if normalized == "y" || normalized == "yes" {
        Ok(ApprovalOutcome::Granted)
    } else {
        Ok(ApprovalOutcome::Denied {
            reason: "approval denied by stdin".into(),
        })
    }
}

fn read_file(
    workspace_root: &Path,
    call_id: platonic_core::ToolCallId,
    input: Value,
) -> AppResult<ToolResult> {
    let input: FileReadInput = serde_json::from_value(input)?;
    let path = resolve_existing_path(workspace_root, &input.path)?;
    let content = fs::read_to_string(&path)?;
    let truncated = content.len() > MAX_READ_BYTES;
    let visible = truncate_utf8(&content, MAX_READ_BYTES);

    Ok(ToolResult {
        call_id,
        summary: format!("read {} bytes from {}", content.len(), input.path),
        data: json!({
            "path": input.path,
            "content": visible,
            "truncated": truncated,
            "bytes": content.len()
        }),
        artifacts: vec![],
        visibility: ResultVisibility::Both,
    })
}

fn list_directory(
    workspace_root: &Path,
    call_id: platonic_core::ToolCallId,
    input: Value,
) -> AppResult<ToolResult> {
    let input: FileListInput = serde_json::from_value(input)?;
    let path = resolve_existing_path(workspace_root, &input.path)?;
    if !path.metadata()?.is_dir() {
        return Err(AppError::Tool(format!("not a directory: {}", input.path)));
    }

    let mut entries = fs::read_dir(&path)?
        .map(|entry| {
            let entry = entry?;
            let file_type = entry.file_type()?;
            Ok(ListEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind: file_kind(&file_type),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    let entry_count = entries.len();
    let mut returned = Vec::new();
    let mut data_bytes = 0usize;
    let mut truncated = false;
    for entry in entries {
        let entry_bytes = estimated_list_entry_bytes(&entry);
        if returned.len() >= MAX_LIST_ENTRIES
            || data_bytes.saturating_add(entry_bytes) > MAX_LIST_DATA_BYTES
        {
            truncated = true;
            break;
        }
        data_bytes += entry_bytes;
        returned.push(entry);
    }
    truncated |= returned.len() < entry_count;
    let returned_count = returned.len();

    Ok(ToolResult {
        call_id,
        summary: format!(
            "listed {} of {} entries in {}",
            returned_count, entry_count, input.path
        ),
        data: json!({
            "path": input.path,
            "entries": returned,
            "truncated": truncated,
            "entry_count": entry_count,
            "returned_count": returned_count
        }),
        artifacts: vec![],
        visibility: ResultVisibility::Both,
    })
}

fn write_file(
    workspace_root: &Path,
    call_id: platonic_core::ToolCallId,
    input: Value,
) -> AppResult<ToolResult> {
    let input: FileWriteInput = serde_json::from_value(input)?;
    let path = resolve_write_path(workspace_root, &input.path)?;
    fs::write(&path, &input.content)?;

    Ok(ToolResult {
        call_id,
        summary: format!("wrote {} bytes to {}", input.content.len(), input.path),
        data: json!({
            "path": input.path,
            "bytes": input.content.len()
        }),
        artifacts: vec![],
        visibility: ResultVisibility::Both,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ListEntry {
    name: String,
    kind: &'static str,
}

fn file_kind(file_type: &fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    }
}

fn estimated_list_entry_bytes(entry: &ListEntry) -> usize {
    entry.name.len() + entry.kind.len() + 32
}

fn resolve_existing_path(workspace_root: &Path, raw_path: &str) -> AppResult<PathBuf> {
    let raw = Path::new(raw_path);
    if path_escapes(raw) {
        return Err(AppError::PathEscapesWorkspace(raw.into()));
    }

    let root = workspace_root.canonicalize()?;
    let candidate = root.join(raw).canonicalize()?;
    if !candidate.starts_with(&root) {
        return Err(AppError::PathEscapesWorkspace(candidate));
    }
    Ok(candidate)
}

fn resolve_write_path(workspace_root: &Path, raw_path: &str) -> AppResult<PathBuf> {
    let raw = Path::new(raw_path);
    if path_escapes(raw) {
        return Err(AppError::PathEscapesWorkspace(raw.into()));
    }

    let root = workspace_root.canonicalize()?;
    let candidate = root.join(raw);
    if let Ok(metadata) = fs::symlink_metadata(&candidate) {
        if metadata.file_type().is_symlink() {
            return Err(AppError::PathEscapesWorkspace(candidate));
        }
        let canonical = candidate.canonicalize()?;
        if !canonical.starts_with(&root) {
            return Err(AppError::PathEscapesWorkspace(canonical));
        }
    }
    let parent = candidate
        .parent()
        .ok_or_else(|| AppError::PathEscapesWorkspace(candidate.clone()))?
        .canonicalize()?;
    if !parent.starts_with(&root) {
        return Err(AppError::PathEscapesWorkspace(parent));
    }
    Ok(candidate)
}

fn path_escapes(path: &Path) -> bool {
    path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
}

fn truncate_utf8(content: &str, max_bytes: usize) -> &str {
    if content.len() <= max_bytes {
        return content;
    }

    let boundary = content
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    &content[..boundary]
}

fn approval_preview(input: &Value) -> String {
    let input = input.to_string();
    if input.chars().count() <= APPROVAL_PREVIEW_CHARS {
        return input;
    }

    let truncated = input
        .chars()
        .take(APPROVAL_PREVIEW_CHARS)
        .collect::<String>();
    format!("{truncated}...(truncated)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use platonic_core::ToolCallId;

    #[test]
    fn read_file_rejects_paths_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.read",
            json!({"path": "../outside.txt"}),
        )
        .unwrap_err();

        assert!(matches!(err, AppError::PathEscapesWorkspace(_)));
    }

    #[test]
    fn write_file_requires_parent_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.write",
            json!({"path": "note.txt", "content": "hello"}),
        )
        .unwrap();

        assert_eq!(result.summary, "wrote 5 bytes to note.txt");
        assert_eq!(
            fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn read_file_truncates_on_utf8_boundary() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "é".repeat(MAX_READ_BYTES)).unwrap();

        let result = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.read",
            json!({"path": "note.txt"}),
        )
        .unwrap();

        let content = result.data["content"].as_str().unwrap();
        assert!(content.is_char_boundary(content.len()));
        assert!(result.data["truncated"].as_bool().unwrap());
    }

    #[test]
    fn list_directory_lists_single_level_entries_in_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.txt"), "b").unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested").join("c.txt"), "c").unwrap();

        let result = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.list",
            json!({"path": "."}),
        )
        .unwrap();

        assert_eq!(result.summary, "listed 3 of 3 entries in .");
        assert_eq!(result.data["truncated"], false);
        assert_eq!(result.data["entry_count"], 3);
        assert_eq!(result.data["returned_count"], 3);
        assert_eq!(
            result.data["entries"],
            json!([
                {"name": "a.txt", "kind": "file"},
                {"name": "b.txt", "kind": "file"},
                {"name": "nested", "kind": "directory"}
            ])
        );
    }

    #[test]
    fn list_directory_rejects_paths_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.list",
            json!({"path": "../outside"}),
        )
        .unwrap_err();

        assert!(matches!(err, AppError::PathEscapesWorkspace(_)));
    }

    #[test]
    fn list_directory_rejects_file_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "hello").unwrap();

        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.list",
            json!({"path": "note.txt"}),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            AppError::Tool(message) if message == "not a directory: note.txt"
        ));
    }

    #[test]
    fn list_directory_truncates_after_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..=MAX_LIST_ENTRIES {
            fs::write(dir.path().join(format!("file_{index:03}.txt")), "x").unwrap();
        }

        let result = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.list",
            json!({"path": "."}),
        )
        .unwrap();

        assert_eq!(result.data["truncated"], true);
        assert_eq!(result.data["entry_count"], MAX_LIST_ENTRIES + 1);
        assert_eq!(result.data["returned_count"], MAX_LIST_ENTRIES);
        assert_eq!(
            result.data["entries"].as_array().unwrap().len(),
            MAX_LIST_ENTRIES
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_directory_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("outside")).unwrap();

        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.list",
            json!({"path": "outside"}),
        )
        .unwrap_err();

        assert!(matches!(err, AppError::PathEscapesWorkspace(_)));
    }

    #[cfg(unix)]
    #[test]
    fn write_file_rejects_existing_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link.txt")).unwrap();

        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.write",
            json!({"path": "link.txt", "content": "hello"}),
        )
        .unwrap_err();

        assert!(matches!(err, AppError::PathEscapesWorkspace(_)));
    }
}
