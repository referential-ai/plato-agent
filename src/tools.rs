use crate::tool_catalog::{FILE_EDIT, FILE_LIST, FILE_READ, FILE_WRITE};
use crate::{AppError, AppResult};
use platonic_core::{ResultVisibility, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, ErrorKind, Write},
    path::{Component, Path, PathBuf},
};

const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_LIST_ENTRIES: usize = 200;
const MAX_LIST_DATA_BYTES: usize = 32 * 1024;
const APPROVAL_PREVIEW_CHARS: usize = 1_000;
const DIFF_PREVIEW_CHARS: usize = 16 * 1024;
const DIFF_TRUNCATED_MARKER: &str = "... diff truncated";

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
struct FileContentInput {
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
        FILE_EDIT => edit_file(workspace_root, call_id, input),
        _ => Err(AppError::Tool(format!("unknown tool: {tool_name}"))),
    }
}

pub fn ask_for_approval(tool_name: &str, input: &Value) -> AppResult<ApprovalOutcome> {
    eprint!("{}", approval_prompt(tool_name, input));
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

pub fn approval_diff_preview(
    workspace_root: &Path,
    tool_name: &str,
    input: &Value,
) -> Option<String> {
    if tool_name != FILE_EDIT {
        return None;
    }

    let input: FileContentInput = serde_json::from_value(input.clone()).ok()?;
    let path = resolve_write_path(workspace_root, &input.path).ok()?;
    let current = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };

    Some(unified_diff(
        &input.path,
        &current,
        &input.content,
        DIFF_PREVIEW_CHARS,
    ))
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
    let input: FileContentInput = serde_json::from_value(input)?;
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

fn edit_file(
    workspace_root: &Path,
    call_id: platonic_core::ToolCallId,
    input: Value,
) -> AppResult<ToolResult> {
    let input: FileContentInput = serde_json::from_value(input)?;
    let path = resolve_write_path(workspace_root, &input.path)?;
    fs::write(&path, &input.content)?;

    Ok(ToolResult {
        call_id,
        summary: format!("edited {} bytes at {}", input.content.len(), input.path),
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

fn approval_prompt(tool_name: &str, input: &Value) -> String {
    let preview = approval_preview(input);
    format!("Approve {tool_name} {preview}? [y/N] ")
}

fn unified_diff(path: &str, current: &str, proposed: &str, max_chars: usize) -> String {
    if current == proposed {
        return String::new();
    }

    let current_lines = diff_lines(current);
    let proposed_lines = diff_lines(proposed);
    let prefix = common_prefix(&current_lines, &proposed_lines);
    let suffix = common_suffix(&current_lines[prefix..], &proposed_lines[prefix..]);
    let context = 3usize;
    let current_changed_end = current_lines.len() - suffix;
    let proposed_changed_end = proposed_lines.len() - suffix;
    let current_start = prefix.saturating_sub(context);
    let proposed_start = prefix.saturating_sub(context);
    let current_end = current_lines.len().min(current_changed_end + context);
    let proposed_end = proposed_lines.len().min(proposed_changed_end + context);
    let current_count = current_end - current_start;
    let proposed_count = proposed_end - proposed_start;

    let mut diff = DiffPreview::new(max_chars);
    diff.push(&format!("--- a/{path}\n"));
    diff.push(&format!("+++ b/{path}\n"));
    diff.push(&format!(
        "@@ -{},{} +{},{} @@\n",
        hunk_start(current_start, current_count),
        current_count,
        hunk_start(proposed_start, proposed_count),
        proposed_count
    ));

    for line in &current_lines[current_start..prefix] {
        diff.push_line(' ', line);
    }
    for line in &current_lines[prefix..current_changed_end] {
        diff.push_line('-', line);
    }
    for line in &proposed_lines[prefix..proposed_changed_end] {
        diff.push_line('+', line);
    }
    for line in &current_lines[current_changed_end..current_end] {
        diff.push_line(' ', line);
    }

    diff.finish()
}

fn diff_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split_inclusive('\n').collect()
    }
}

fn common_prefix(left: &[&str], right: &[&str]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix(left: &[&str], right: &[&str]) -> usize {
    let max = left.len().min(right.len());
    let mut count = 0usize;
    while count < max && left[left.len() - count - 1] == right[right.len() - count - 1] {
        count += 1;
    }
    count
}

fn hunk_start(start: usize, count: usize) -> usize {
    if count == 0 { start } else { start + 1 }
}

struct DiffPreview {
    value: String,
    max_chars: usize,
    chars: usize,
    truncated: bool,
}

impl DiffPreview {
    fn new(max_chars: usize) -> Self {
        Self {
            value: String::new(),
            max_chars,
            chars: 0,
            truncated: false,
        }
    }

    fn push_line(&mut self, prefix: char, line: &str) {
        self.push(&prefix.to_string());
        self.push(line);
        if !line.ends_with('\n') {
            self.push("\n");
        }
    }

    fn push(&mut self, content: &str) {
        if self.truncated {
            return;
        }

        let remaining = self.max_chars.saturating_sub(self.chars);
        let content_chars = content.chars().count();
        if content_chars <= remaining {
            self.value.push_str(content);
            self.chars += content_chars;
            return;
        }

        self.value.extend(content.chars().take(remaining));
        self.chars = self.max_chars;
        self.mark_truncated();
    }

    fn mark_truncated(&mut self) {
        if self.truncated {
            return;
        }
        if !self.value.ends_with('\n') {
            self.value.push('\n');
        }
        self.value.push_str(DIFF_TRUNCATED_MARKER);
        self.value.push('\n');
        self.truncated = true;
    }

    fn finish(self) -> String {
        self.value
    }
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
    fn edit_file_writes_full_proposed_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "old").unwrap();

        let result = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.edit",
            json!({"path": "note.txt", "content": "new"}),
        )
        .unwrap();

        assert_eq!(result.summary, "edited 3 bytes at note.txt");
        assert_eq!(
            fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn edit_file_rejects_paths_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.edit",
            json!({"path": "../outside.txt", "content": "hello"}),
        )
        .unwrap_err();

        assert!(matches!(err, AppError::PathEscapesWorkspace(_)));
    }

    #[test]
    fn edit_file_rejects_unknown_input_fields() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.edit",
            json!({"path": "note.txt", "content": "hello", "anchor": "old"}),
        )
        .unwrap_err();

        assert!(matches!(err, AppError::Json(_)));
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

    #[cfg(unix)]
    #[test]
    fn edit_file_rejects_existing_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link.txt")).unwrap();

        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            "file.edit",
            json!({"path": "link.txt", "content": "hello"}),
        )
        .unwrap_err();

        assert!(matches!(err, AppError::PathEscapesWorkspace(_)));
    }

    #[test]
    fn file_edit_diff_preview_shows_current_vs_proposed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "old\nsame\n").unwrap();

        let diff = approval_diff_preview(
            dir.path(),
            FILE_EDIT,
            &json!({"path": "note.txt", "content": "new\nsame\n"}),
        )
        .unwrap();

        assert!(diff.contains("--- a/note.txt"));
        assert!(diff.contains("+++ b/note.txt"));
        assert!(diff.contains("-old\n"));
        assert!(diff.contains("+new\n"));
        assert!(diff.contains(" same\n"));
    }

    #[test]
    fn file_edit_diff_preview_for_missing_file_is_whole_file_add() {
        let dir = tempfile::tempdir().unwrap();

        let diff = approval_diff_preview(
            dir.path(),
            FILE_EDIT,
            &json!({"path": "created.txt", "content": "hello\n"}),
        )
        .unwrap();

        assert!(diff.contains("@@ -0,0 +1,1 @@"));
        assert!(diff.contains("+hello\n"));
    }

    #[test]
    fn file_edit_diff_preview_truncates_huge_diff_with_marker() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "old\n").unwrap();

        let diff = approval_diff_preview(
            dir.path(),
            FILE_EDIT,
            &json!({"path": "note.txt", "content": "new\n".repeat(DIFF_PREVIEW_CHARS)}),
        )
        .unwrap();

        assert!(diff.contains(DIFF_TRUNCATED_MARKER));
        assert!(diff.chars().count() < DIFF_PREVIEW_CHARS + DIFF_TRUNCATED_MARKER.len() + 4);
    }

    #[test]
    fn stdin_approval_prompt_keeps_json_preview_for_file_edit() {
        let prompt = approval_prompt(FILE_EDIT, &json!({"path": "note.txt", "content": "new\n"}));

        assert!(prompt.contains(r#""path":"note.txt""#));
        assert!(prompt.contains(r#""content":"new\n""#));
        assert!(!prompt.contains("--- a/note.txt"));
    }
}
