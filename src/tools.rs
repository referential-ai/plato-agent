use crate::tool_catalog::{FILE_EDIT, FILE_LIST, FILE_READ, FILE_WRITE, SHELL_EXEC};
use crate::{AppError, AppResult};
use platonic_core::{ResultVisibility, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, ErrorKind, Read, Write},
    os::unix::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_LIST_ENTRIES: usize = 200;
const MAX_LIST_DATA_BYTES: usize = 32 * 1024;
const SHELL_OUTPUT_BYTES: usize = 32 * 1024;
const SHELL_OUTPUT_TRUNCATED_MARKER: &str = "\n... output truncated";
const SHELL_DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const SHELL_MAX_TIMEOUT_SECONDS: u64 = 600;
const APPROVAL_PREVIEW_CHARS: usize = 1_000;
const DIFF_PREVIEW_CHARS: usize = 16 * 1024;
const DIFF_TRUNCATED_MARKER: &str = "... diff truncated";
const SHELL_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellExecInput {
    command: String,
    timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalOutcome {
    Granted,
    Denied { reason: String },
}

#[derive(Clone, Copy, Debug)]
pub struct ToolExecutionContext<'a> {
    pub workspace_root: &'a Path,
    pub provider_api_key_env: Option<&'a str>,
    pub cancel: Option<&'a AtomicBool>,
}

impl<'a> ToolExecutionContext<'a> {
    pub fn new(workspace_root: &'a Path) -> Self {
        Self {
            workspace_root,
            provider_api_key_env: None,
            cancel: None,
        }
    }
}

pub fn execute_tool(
    workspace_root: &Path,
    call_id: platonic_core::ToolCallId,
    tool_name: &str,
    input: Value,
) -> AppResult<ToolResult> {
    execute_tool_with_context(
        ToolExecutionContext::new(workspace_root),
        call_id,
        tool_name,
        input,
    )
}

pub fn execute_tool_with_context(
    context: ToolExecutionContext<'_>,
    call_id: platonic_core::ToolCallId,
    tool_name: &str,
    input: Value,
) -> AppResult<ToolResult> {
    match tool_name {
        FILE_READ => read_file(context.workspace_root, call_id, input),
        FILE_LIST => list_directory(context.workspace_root, call_id, input),
        FILE_WRITE => write_file(context.workspace_root, call_id, input, "wrote", "to"),
        FILE_EDIT => write_file(context.workspace_root, call_id, input, "edited", "at"),
        SHELL_EXEC => shell_exec(context, call_id, input),
        _ => Err(AppError::Tool(format!("unknown tool: {tool_name}"))),
    }
}

pub fn ask_for_approval(
    tool_name: &str,
    input: &Value,
    approval_preview: Option<&str>,
) -> AppResult<ApprovalOutcome> {
    eprint!("{}", approval_prompt(tool_name, input, approval_preview));
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

pub fn approval_command_preview(
    workspace_root: &Path,
    tool_name: &str,
    input: &Value,
    provider_api_key_env: Option<&str>,
) -> Option<String> {
    if tool_name != SHELL_EXEC {
        return None;
    }

    let input: ShellExecInput = serde_json::from_value(input.clone()).ok()?;
    let timeout_seconds = normalize_timeout_seconds(input.timeout_seconds);
    let cwd = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let provider = provider_api_key_env.unwrap_or("configured provider key");
    Some(format!(
        "command: {}\ncwd: {}\ntimeout: {}s\neffect: ExternalSideEffect\nenv: scrubbed allowlist; credential-like names and {provider} removed",
        input.command,
        cwd.display(),
        timeout_seconds
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
    summary_verb: &str,
    summary_preposition: &str,
) -> AppResult<ToolResult> {
    let input: FileContentInput = serde_json::from_value(input)?;
    let path = resolve_write_path(workspace_root, &input.path)?;
    fs::write(&path, &input.content)?;

    Ok(ToolResult {
        call_id,
        summary: format!(
            "{summary_verb} {} bytes {summary_preposition} {}",
            input.content.len(),
            input.path
        ),
        data: json!({
            "path": input.path,
            "bytes": input.content.len()
        }),
        artifacts: vec![],
        visibility: ResultVisibility::Both,
    })
}

fn shell_exec(
    context: ToolExecutionContext<'_>,
    call_id: platonic_core::ToolCallId,
    input: Value,
) -> AppResult<ToolResult> {
    let input: ShellExecInput = serde_json::from_value(input)?;
    if input.command.trim().is_empty() {
        return Err(AppError::Tool("shell.exec command is empty".into()));
    }
    let timeout_seconds = normalize_timeout_seconds(input.timeout_seconds);
    let cwd = context.workspace_root.canonicalize()?;
    let env = shell_child_env(context.provider_api_key_env);
    let started = Instant::now();
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&input.command)
        .current_dir(&cwd)
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Tool("shell.exec stdout pipe unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Tool("shell.exec stderr pipe unavailable".into()))?;
    let stdout_reader = thread::spawn(move || read_capped_output(stdout, SHELL_OUTPUT_BYTES));
    let stderr_reader = thread::spawn(move || read_capped_output(stderr, SHELL_OUTPUT_BYTES));
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let mut timed_out = false;
    let mut canceled = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if context
            .cancel
            .is_some_and(|cancel| cancel.load(Ordering::SeqCst))
        {
            canceled = true;
            kill_shell_group(&mut child);
            break child.wait()?;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_shell_group(&mut child);
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = join_output_reader(stdout_reader)?;
    let stderr = join_output_reader(stderr_reader)?;
    let duration_ms = started.elapsed().as_millis() as u64;

    if timed_out {
        return Err(AppError::Tool(format!(
            "shell.exec timed out after {timeout_seconds}s"
        )));
    }
    if canceled {
        return Err(AppError::Tool("shell.exec canceled".into()));
    }

    let exit_code = status.code();
    let exit_label = exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".into());
    Ok(ToolResult {
        call_id,
        summary: format!("shell.exec exited {exit_label} in {duration_ms}ms"),
        data: json!({
            "command": input.command,
            "cwd": cwd.to_string_lossy(),
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "stdout": stdout.text,
            "stderr": stderr.text,
            "stdout_truncated": stdout.truncated,
            "stderr_truncated": stderr.truncated
        }),
        artifacts: vec![],
        visibility: ResultVisibility::Both,
    })
}

fn kill_shell_group(child: &mut Child) {
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
}

fn normalize_timeout_seconds(timeout_seconds: Option<u64>) -> u64 {
    timeout_seconds
        .unwrap_or(SHELL_DEFAULT_TIMEOUT_SECONDS)
        .clamp(1, SHELL_MAX_TIMEOUT_SECONDS)
}

fn shell_child_env(provider_api_key_env: Option<&str>) -> Vec<(String, String)> {
    shell_child_env_from(env::vars(), provider_api_key_env)
}

fn shell_child_env_from(
    vars: impl IntoIterator<Item = (String, String)>,
    provider_api_key_env: Option<&str>,
) -> Vec<(String, String)> {
    vars.into_iter()
        .filter(|(name, _)| SHELL_ENV_ALLOWLIST.contains(&name.as_str()))
        .filter(|(name, _)| !is_credential_env_name(name))
        .filter(|(name, _)| provider_api_key_env != Some(name.as_str()))
        .collect()
}

fn is_credential_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
        .iter()
        .any(|needle| upper.contains(needle))
}

#[derive(Debug, Eq, PartialEq)]
struct CappedOutput {
    text: String,
    truncated: bool,
}

fn read_capped_output(mut reader: impl Read, max_bytes: usize) -> io::Result<CappedOutput> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(bytes.len());
        if remaining == 0 {
            truncated = true;
            continue;
        }
        let take = remaining.min(read);
        bytes.extend_from_slice(&buffer[..take]);
        if take < read {
            truncated = true;
        }
    }

    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str(SHELL_OUTPUT_TRUNCATED_MARKER);
    }
    Ok(CappedOutput { text, truncated })
}

fn join_output_reader(
    reader: thread::JoinHandle<io::Result<CappedOutput>>,
) -> AppResult<CappedOutput> {
    reader
        .join()
        .map_err(|_| AppError::Tool("shell.exec output reader panicked".into()))?
        .map_err(AppError::from)
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

pub fn approval_input_preview(input: &Value) -> String {
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

fn approval_prompt(tool_name: &str, input: &Value, approval_preview: Option<&str>) -> String {
    if let Some(approval_preview) = approval_preview {
        return format!("Approve {tool_name}?\n{approval_preview}\n[y/N] ");
    }

    let preview = approval_input_preview(input);
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
    push_changed_lines(
        &mut diff,
        &current_lines[prefix..current_changed_end],
        &proposed_lines[prefix..proposed_changed_end],
    );
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

fn push_changed_lines(diff: &mut DiffPreview, current: &[&str], proposed: &[&str]) {
    let mut current_index = 0usize;
    let mut proposed_index = 0usize;

    while current_index < current.len() || proposed_index < proposed.len() {
        match (current.get(current_index), proposed.get(proposed_index)) {
            (Some(current_line), Some(proposed_line)) if current_line == proposed_line => {
                diff.push_line(' ', current_line);
                current_index += 1;
                proposed_index += 1;
            }
            (Some(current_line), Some(proposed_line)) => {
                diff.push_line('-', current_line);
                diff.push_line('+', proposed_line);
                current_index += 1;
                proposed_index += 1;
            }
            (Some(current_line), None) => {
                diff.push_line('-', current_line);
                current_index += 1;
            }
            (None, Some(proposed_line)) => {
                diff.push_line('+', proposed_line);
                proposed_index += 1;
            }
            (None, None) => break,
        }
    }
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
    fn approval_input_preview_is_bounded() {
        let preview = approval_input_preview(&json!({"content": "x".repeat(2_000)}));

        assert!(preview.ends_with("...(truncated)"));
        assert_eq!(
            preview
                .strip_suffix("...(truncated)")
                .unwrap()
                .chars()
                .count(),
            APPROVAL_PREVIEW_CHARS
        );
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
    fn file_edit_diff_preview_skips_unreadable_current_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("note.txt")).unwrap();

        let diff = approval_diff_preview(
            dir.path(),
            FILE_EDIT,
            &json!({"path": "note.txt", "content": "hello\n"}),
        );

        assert_eq!(diff, None);
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
    fn file_edit_diff_preview_truncation_keeps_proposed_content() {
        let dir = tempfile::tempdir().unwrap();
        let middle = (0..2000)
            .map(|line| format!("same-{line:04}\n"))
            .collect::<String>();
        fs::write(
            dir.path().join("note.txt"),
            format!("old top\n{middle}old bottom\n"),
        )
        .unwrap();

        let diff = approval_diff_preview(
            dir.path(),
            FILE_EDIT,
            &json!({"path": "note.txt", "content": format!("new top\n{middle}new bottom\n")}),
        )
        .unwrap();

        assert!(diff.contains("-old top\n"));
        assert!(diff.contains("+new top\n"));
        assert!(diff.contains(" same-0000\n"));
        assert!(diff.contains(DIFF_TRUNCATED_MARKER));
        assert!(diff.find("+new top").unwrap() < diff.find(DIFF_TRUNCATED_MARKER).unwrap());
    }

    #[test]
    fn stdin_approval_prompt_keeps_json_preview_for_file_edit() {
        let prompt = approval_prompt(
            FILE_EDIT,
            &json!({"path": "note.txt", "content": "new\n"}),
            None,
        );

        assert!(prompt.contains(r#""path":"note.txt""#));
        assert!(prompt.contains(r#""content":"new\n""#));
        assert!(!prompt.contains("--- a/note.txt"));
    }

    #[test]
    fn shell_approval_preview_includes_command_cwd_timeout_effect_and_env_posture() {
        let dir = tempfile::tempdir().unwrap();
        let preview = approval_command_preview(
            dir.path(),
            SHELL_EXEC,
            &json!({"command": "cargo test", "timeout_seconds": 700}),
            Some("OPENROUTER_API_KEY"),
        )
        .unwrap();

        assert!(preview.contains("command: cargo test"));
        assert!(preview.contains(&format!(
            "cwd: {}",
            dir.path().canonicalize().unwrap().display()
        )));
        assert!(preview.contains("timeout: 600s"));
        assert!(preview.contains("effect: ExternalSideEffect"));
        assert!(preview.contains("env: scrubbed allowlist"));
        assert!(preview.contains("OPENROUTER_API_KEY removed"));
    }

    #[test]
    fn shell_timeout_defaults_and_clamps() {
        assert_eq!(normalize_timeout_seconds(None), 120);
        assert_eq!(normalize_timeout_seconds(Some(0)), 1);
        assert_eq!(normalize_timeout_seconds(Some(10)), 10);
        assert_eq!(normalize_timeout_seconds(Some(700)), 600);
    }

    #[test]
    fn shell_env_keeps_only_allowlisted_non_credentials() {
        let env = shell_child_env_from(
            vec![
                ("PATH".into(), "/bin".into()),
                ("HOME".into(), "/home/user".into()),
                ("OPENROUTER_API_KEY".into(), "secret".into()),
                ("CARGO_AUTH_TOKEN".into(), "secret".into()),
                ("HTTP_PROXY".into(), "http://proxy".into()),
                ("RUSTUP_HOME".into(), "/rustup".into()),
            ],
            Some("OPENROUTER_API_KEY"),
        );

        assert_eq!(
            env,
            vec![
                ("PATH".into(), "/bin".into()),
                ("HOME".into(), "/home/user".into()),
                ("RUSTUP_HOME".into(), "/rustup".into())
            ]
        );
    }

    #[test]
    fn capped_output_marks_truncation() {
        let output = read_capped_output(io::Cursor::new(b"abcdef".to_vec()), 3).unwrap();

        assert_eq!(output.text, format!("abc{SHELL_OUTPUT_TRUNCATED_MARKER}"));
        assert!(output.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn shell_exec_runs_from_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            SHELL_EXEC,
            json!({"command": "pwd"}),
        )
        .unwrap();

        assert_eq!(result.data["exit_code"], 0);
        let cwd = dir.path().canonicalize().unwrap();
        assert_eq!(result.data["cwd"].as_str().unwrap(), cwd.to_string_lossy());
        assert_eq!(
            result.data["stdout"].as_str().unwrap().trim(),
            cwd.to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_exec_records_nonzero_exit_as_finished_result() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            SHELL_EXEC,
            json!({"command": "printf fail >&2; exit 7"}),
        )
        .unwrap();

        assert_eq!(result.data["exit_code"], 7);
        assert_eq!(result.data["stderr"], "fail");
        assert!(result.data.get("timed_out").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn shell_exec_caps_stdout_and_stderr_independently() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            SHELL_EXEC,
            json!({"command": "yes out | head -c 33000; yes err | head -c 33000 >&2"}),
        )
        .unwrap();

        assert!(result.data["stdout"].as_str().unwrap().len() > SHELL_OUTPUT_BYTES);
        assert!(result.data["stderr"].as_str().unwrap().len() > SHELL_OUTPUT_BYTES);
        assert!(
            result.data["stdout"]
                .as_str()
                .unwrap()
                .contains(SHELL_OUTPUT_TRUNCATED_MARKER)
        );
        assert!(
            result.data["stderr"]
                .as_str()
                .unwrap()
                .contains(SHELL_OUTPUT_TRUNCATED_MARKER)
        );
        assert_eq!(result.data["stdout_truncated"], true);
        assert_eq!(result.data["stderr_truncated"], true);
    }

    #[cfg(unix)]
    #[test]
    fn shell_exec_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            SHELL_EXEC,
            json!({"command": "sleep 2", "timeout_seconds": 1}),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            AppError::Tool(message) if message == "shell.exec timed out after 1s"
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shell_exec_timeout_kills_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let command = format!(
            "sleep 30 >/dev/null 2>&1 & echo $! > {}; wait",
            pid_file.display()
        );
        let started = Instant::now();
        let err = execute_tool(
            dir.path(),
            ToolCallId::new("call_1").unwrap(),
            SHELL_EXEC,
            json!({"command": command, "timeout_seconds": 1}),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            AppError::Tool(message) if message == "shell.exec timed out after 1s"
        ));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout return blocked on surviving grandchild"
        );

        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => break,
                Ok(stat) if stat.split_whitespace().nth(2) == Some("Z") => break,
                Ok(_) => {}
            }
            assert!(
                Instant::now() < deadline,
                "grandchild {pid} survived group kill"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    #[test]
    fn shell_exec_observes_cancel_flag() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let err = execute_tool_with_context(
            ToolExecutionContext {
                workspace_root: dir.path(),
                provider_api_key_env: None,
                cancel: Some(&cancel),
            },
            ToolCallId::new("call_1").unwrap(),
            SHELL_EXEC,
            json!({"command": "sleep 5"}),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            AppError::Tool(message) if message == "shell.exec canceled"
        ));
    }
}
