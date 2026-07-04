use crate::{AppError, AppResult};
use platonic_core::{EffectClass, ResultVisibility, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

const MAX_READ_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileReadInput {
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

pub fn effect_for_tool(name: &str) -> EffectClass {
    match name {
        "file.read" => EffectClass::ReadOnly,
        "file.write" => EffectClass::WorkspaceWrite,
        _ => EffectClass::ExternalSideEffect,
    }
}

pub fn execute_tool(
    workspace_root: &Path,
    call_id: platonic_core::ToolCallId,
    tool_name: &str,
    input: Value,
) -> AppResult<ToolResult> {
    match tool_name {
        "file.read" => read_file(workspace_root, call_id, input),
        "file.write" => write_file(workspace_root, call_id, input),
        _ => Err(AppError::Tool(format!("unknown tool: {tool_name}"))),
    }
}

pub fn ask_for_approval(tool_name: &str, input: &Value) -> AppResult<ApprovalOutcome> {
    eprint!("Approve {tool_name} {input}? [y/N] ");
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
}
