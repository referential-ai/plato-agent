use plato_agent::{
    daemon::{
        client::{DaemonClient, DaemonConnectionConfig},
        protocol::{
            ApprovalDecisionName, HelloResult, RunStateName, SessionSummary, TranscriptReadResult,
            TypedRun, TypedTranscriptEntry,
        },
    },
    paths,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, io::ErrorKind, path::Path, path::PathBuf};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

const REQUIRED_CAPABILITIES: [&str; 4] = [
    "hello",
    "sessions.list",
    "transcript.read",
    "transcript.read.typed",
];

struct DesktopState {
    workspace_file: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum BootstrapView {
    NeedsWorkspace {
        reason: Option<String>,
    },
    Ready {
        workspace_root: String,
        daemon_version: String,
        sessions: Vec<DesktopSession>,
        selected_run: Option<DesktopRun>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSession {
    session_id: String,
    run_id: String,
    status: RunStateName,
    latest_question: String,
}

impl From<SessionSummary> for DesktopSession {
    fn from(session: SessionSummary) -> Self {
        Self {
            session_id: session.session_id,
            run_id: session.run_id,
            status: session.status,
            latest_question: session.latest_question,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopRun {
    run_id: String,
    session_index: u64,
    status: RunStateName,
    entries: Vec<DesktopEntry>,
}

impl From<TypedRun> for DesktopRun {
    fn from(run: TypedRun) -> Self {
        Self {
            run_id: run.run_id,
            session_index: run.session_index,
            status: run.status,
            entries: run.entries.into_iter().map(DesktopEntry::from).collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum DesktopEntry {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        call_id: String,
        tool: String,
        input: Value,
    },
    ToolResult {
        call_id: String,
        summary: String,
    },
    Approval {
        call_id: String,
        decision: ApprovalDecisionName,
        actor_id: String,
        reason: Option<String>,
    },
    PolicyDenied {
        call_id: String,
        reason: String,
    },
    ToolFailed {
        call_id: String,
        error: String,
    },
}

impl From<TypedTranscriptEntry> for DesktopEntry {
    fn from(entry: TypedTranscriptEntry) -> Self {
        match entry {
            TypedTranscriptEntry::User { text } => Self::User { text },
            TypedTranscriptEntry::Assistant { text } => Self::Assistant { text },
            TypedTranscriptEntry::ToolCall {
                call_id,
                tool,
                input,
            } => Self::ToolCall {
                call_id,
                tool,
                input,
            },
            TypedTranscriptEntry::ToolResult { call_id, summary } => {
                Self::ToolResult { call_id, summary }
            }
            TypedTranscriptEntry::Approval {
                call_id,
                decision,
                actor_id,
                reason,
            } => Self::Approval {
                call_id,
                decision,
                actor_id,
                reason,
            },
            TypedTranscriptEntry::PolicyDenied { call_id, reason } => {
                Self::PolicyDenied { call_id, reason }
            }
            TypedTranscriptEntry::ToolFailed { call_id, error } => {
                Self::ToolFailed { call_id, error }
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedWorkspace {
    workspace_root: String,
}

enum SavedWorkspaceState {
    Missing,
    Invalid(String),
    Ready(PathBuf),
}

#[tauri::command]
async fn bootstrap(state: tauri::State<'_, DesktopState>) -> Result<BootstrapView, String> {
    let workspace_file = state.workspace_file.clone();
    tauri::async_runtime::spawn_blocking(move || bootstrap_from_store(&workspace_file, None))
        .await
        .map_err(|error| format!("Desktop worker failed: {error}"))?
}

#[tauri::command]
async fn pick_workspace(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopState>,
) -> Result<Option<BootstrapView>, String> {
    let selected =
        tauri::async_runtime::spawn_blocking(move || app.dialog().file().blocking_pick_folder())
            .await
            .map_err(|error| format!("Workspace picker failed: {error}"))?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let selected = selected
        .into_path()
        .map_err(|error| format!("Workspace picker returned an invalid path: {error}"))?;
    let workspace_file = state.workspace_file.clone();
    tauri::async_runtime::spawn_blocking(move || {
        persist_workspace(&workspace_file, &selected)?;
        connect_workspace(&selected, None).map(Some)
    })
    .await
    .map_err(|error| format!("Desktop worker failed: {error}"))?
}

#[tauri::command]
async fn read_run(
    run_id: String,
    state: tauri::State<'_, DesktopState>,
) -> Result<DesktopRun, String> {
    let workspace_file = state.workspace_file.clone();
    tauri::async_runtime::spawn_blocking(move || {
        read_run_from_store(&workspace_file, &run_id, None)
    })
    .await
    .map_err(|error| format!("Desktop worker failed: {error}"))?
}

fn bootstrap_from_store(
    workspace_file: &Path,
    socket_path: Option<PathBuf>,
) -> Result<BootstrapView, String> {
    match load_saved_workspace(workspace_file) {
        SavedWorkspaceState::Missing => Ok(BootstrapView::NeedsWorkspace { reason: None }),
        SavedWorkspaceState::Invalid(reason) => Ok(BootstrapView::NeedsWorkspace {
            reason: Some(reason),
        }),
        SavedWorkspaceState::Ready(workspace_root) => {
            connect_workspace(&workspace_root, socket_path)
        }
    }
}

fn connect_workspace(
    workspace_root: &Path,
    socket_path: Option<PathBuf>,
) -> Result<BootstrapView, String> {
    let config = DaemonConnectionConfig::resolve(workspace_root, socket_path)
        .map_err(|error| format!("Workspace is invalid: {error}"))?;
    let mut client = DaemonClient::connect(&config.socket_path)
        .map_err(|error| format!("Unable to connect to plato-agentd: {error}"))?;
    let hello = client
        .hello(&config.workspace_root)
        .map_err(|error| format!("Daemon hello failed: {error}"))?;
    validate_hello(&config.workspace_root, &hello)?;
    let daemon_version = hello.daemon_version;
    let session_summaries = client
        .sessions_list()
        .map_err(|error| format!("Unable to list daemon sessions: {error}"))?;
    let selected_run = session_summaries
        .first()
        .map(|session| read_typed_run(&mut client, &session.run_id))
        .transpose()?;
    Ok(BootstrapView::Ready {
        workspace_root: config.workspace_root.to_string_lossy().into_owned(),
        daemon_version,
        sessions: session_summaries
            .into_iter()
            .map(DesktopSession::from)
            .collect(),
        selected_run,
    })
}

fn read_run_from_store(
    workspace_file: &Path,
    run_id: &str,
    socket_path: Option<PathBuf>,
) -> Result<DesktopRun, String> {
    let SavedWorkspaceState::Ready(workspace_root) = load_saved_workspace(workspace_file) else {
        return Err("No valid workspace is selected".into());
    };
    let config = DaemonConnectionConfig::resolve(&workspace_root, socket_path)
        .map_err(|error| format!("Workspace is invalid: {error}"))?;
    let mut client = DaemonClient::connect(&config.socket_path)
        .map_err(|error| format!("Unable to connect to plato-agentd: {error}"))?;
    let hello = client
        .hello(&config.workspace_root)
        .map_err(|error| format!("Daemon hello failed: {error}"))?;
    validate_hello(&config.workspace_root, &hello)?;
    read_typed_run(&mut client, run_id)
}

fn validate_hello(workspace_root: &Path, hello: &HelloResult) -> Result<(), String> {
    let expected_workspace_id = paths::workspace_id(workspace_root)
        .map_err(|error| format!("Workspace is invalid: {error}"))?;
    if hello.workspace_id != expected_workspace_id {
        return Err(format!(
            "Incompatible daemon: expected workspace {expected_workspace_id}, got {}",
            hello.workspace_id
        ));
    }
    require_capabilities(&hello.capabilities)
}

fn require_capabilities(capabilities: &[String]) -> Result<(), String> {
    if let Some(missing) = REQUIRED_CAPABILITIES.iter().find(|required| {
        !capabilities
            .iter()
            .any(|capability| capability == **required)
    }) {
        return Err(format!(
            "Incompatible daemon: missing required capability {missing}"
        ));
    }
    Ok(())
}

fn read_typed_run(client: &mut DaemonClient, run_id: &str) -> Result<DesktopRun, String> {
    let transcript = client
        .transcript_read(run_id)
        .map_err(|error| format!("Unable to read run {run_id}: {error}"))?;
    extract_typed_run(run_id, transcript)
}

fn extract_typed_run(
    expected_run_id: &str,
    transcript: TranscriptReadResult,
) -> Result<DesktopRun, String> {
    let typed = transcript.typed.ok_or_else(|| {
        "Incompatible daemon: transcript.read returned no typed payload".to_string()
    })?;
    if typed.runs.len() != 1 {
        return Err(format!(
            "Incompatible daemon: exact-run transcript returned {} runs",
            typed.runs.len()
        ));
    }
    let run = typed.runs.into_iter().next().expect("length checked");
    if run.run_id != expected_run_id {
        return Err(format!(
            "Incompatible daemon: requested run {expected_run_id}, got {}",
            run.run_id
        ));
    }
    Ok(DesktopRun::from(run))
}

fn load_saved_workspace(workspace_file: &Path) -> SavedWorkspaceState {
    let bytes = match fs::read(workspace_file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return SavedWorkspaceState::Missing,
        Err(error) => {
            return SavedWorkspaceState::Invalid(format!(
                "Saved workspace could not be read: {error}"
            ));
        }
    };
    let saved = match serde_json::from_slice::<SavedWorkspace>(&bytes) {
        Ok(saved) => saved,
        Err(_) => {
            return SavedWorkspaceState::Invalid("Saved workspace is invalid".into());
        }
    };
    let path = PathBuf::from(saved.workspace_root);
    match path.canonicalize() {
        Ok(path) if path.is_dir() => SavedWorkspaceState::Ready(path),
        _ => SavedWorkspaceState::Invalid("Saved workspace no longer exists".into()),
    }
}

fn persist_workspace(workspace_file: &Path, workspace_root: &Path) -> Result<PathBuf, String> {
    let canonical = workspace_root
        .canonicalize()
        .map_err(|error| format!("Workspace cannot be resolved: {error}"))?;
    if !canonical.is_dir() {
        return Err("Selected workspace is not a directory".into());
    }
    let workspace_root = canonical
        .to_str()
        .ok_or_else(|| "Workspace path must be valid UTF-8".to_string())?;
    if let Some(parent) = workspace_file.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Workspace selection could not be saved: {error}"))?;
    }
    let temporary = workspace_file.with_extension("json.tmp");
    let bytes = serde_json::to_vec(&SavedWorkspace {
        workspace_root: workspace_root.into(),
    })
    .map_err(|error| format!("Workspace selection could not be encoded: {error}"))?;
    fs::write(&temporary, bytes)
        .map_err(|error| format!("Workspace selection could not be saved: {error}"))?;
    fs::rename(&temporary, workspace_file)
        .map_err(|error| format!("Workspace selection could not be saved: {error}"))?;
    Ok(canonical)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let workspace_file = app.path().app_data_dir()?.join("workspace.json");
            app.manage(DesktopState { workspace_file });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            pick_workspace,
            read_run
        ])
        .run(tauri::generate_context!())
        .expect("error while running Plato desktop");
}

#[cfg(test)]
mod tests {
    use super::*;
    use plato_agent::daemon::protocol::{Envelope, EnvelopeKind, PROTOCOL_VERSION};
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::{UnixListener, UnixStream},
        thread,
    };

    #[test]
    fn missing_invalid_and_persisted_workspaces_have_stable_states() {
        let state = tempfile::tempdir().unwrap();
        let workspace_file = state.path().join("app/workspace.json");
        assert!(matches!(
            load_saved_workspace(&workspace_file),
            SavedWorkspaceState::Missing
        ));
        assert_eq!(
            bootstrap_from_store(&workspace_file, None).unwrap(),
            BootstrapView::NeedsWorkspace { reason: None }
        );

        let workspace = tempfile::tempdir().unwrap();
        let canonical = persist_workspace(&workspace_file, workspace.path()).unwrap();
        assert!(matches!(
            load_saved_workspace(&workspace_file),
            SavedWorkspaceState::Ready(path) if path == canonical
        ));

        drop(workspace);
        assert!(matches!(
            load_saved_workspace(&workspace_file),
            SavedWorkspaceState::Invalid(reason) if reason == "Saved workspace no longer exists"
        ));
        assert_eq!(
            bootstrap_from_store(&workspace_file, None).unwrap(),
            BootstrapView::NeedsWorkspace {
                reason: Some("Saved workspace no longer exists".into())
            }
        );
    }

    #[test]
    fn files_cannot_be_persisted_as_workspaces() {
        let state = tempfile::tempdir().unwrap();
        let workspace_file = state.path().join("workspace.json");
        let file = state.path().join("not-a-workspace");
        fs::write(&file, "text").unwrap();

        let error = persist_workspace(&workspace_file, &file).unwrap_err();

        assert_eq!(error, "Selected workspace is not a directory");
        assert!(!workspace_file.exists());
    }

    #[test]
    fn capability_manifest_exposes_only_the_three_typed_commands() {
        let capability: Value =
            serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
        assert_eq!(
            capability["permissions"],
            json!(["allow-bootstrap", "allow-pick-workspace", "allow-read-run"])
        );
        let serialized = serde_json::to_string(&capability).unwrap();
        for forbidden in ["dialog:", "fs:", "shell:", "http:", "core:", "remote"] {
            assert!(!serialized.contains(forbidden), "found {forbidden}");
        }
        let build = include_str!("../build.rs");
        for command in ["bootstrap", "pick_workspace", "read_run"] {
            assert!(build.contains(&format!("\"{command}\"")));
        }
    }

    #[test]
    fn desktop_bridge_returns_only_typed_presentation_data() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let workspace_id = paths::workspace_id(workspace.path()).unwrap();
        let expected_workspace_id = workspace_id.clone();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let hello = read_request(&mut reader);
            write_response(
                &mut writer,
                hello.id,
                "hello",
                json!({
                    "daemon_version": "0.1.0",
                    "workspace_id": expected_workspace_id,
                    "ledger_path": "/secret/ledger.db",
                    "capabilities": REQUIRED_CAPABILITIES
                }),
            );
            let sessions = read_request(&mut reader);
            write_response(
                &mut writer,
                sessions.id,
                "sessions.list",
                json!({
                    "sessions": [{
                        "session_id": "session_1",
                        "run_id": "run_1",
                        "status": "finished",
                        "latest_question": "hello",
                        "ledger_path": "/secret/ledger.db"
                    }]
                }),
            );
            let transcript = read_request(&mut reader);
            assert_eq!(transcript.params.unwrap()["run_id"], "run_1");
            write_response(
                &mut writer,
                transcript.id,
                "transcript.read",
                json!({
                    "run_id": "run_1",
                    "status": "finished",
                    "final_answer": "hi",
                    "transcript": "POISON_LEGACY_TRANSCRIPT",
                    "typed": {"runs": [{
                        "run_id": "run_1",
                        "session_index": 0,
                        "status": "finished",
                        "entries": [
                            {"kind": "user", "text": "hello"},
                            {"kind": "assistant", "text": "hi"}
                        ]
                    }]}
                }),
            );
        });

        let view = connect_workspace(workspace.path(), Some(socket_path)).unwrap();
        handle.join().unwrap();
        let serialized = serde_json::to_string(&view).unwrap();

        assert!(serialized.contains(workspace.path().to_str().unwrap()));
        assert!(serialized.contains("\"kind\":\"assistant\""));
        for forbidden in [
            "POISON_LEGACY_TRANSCRIPT",
            "/secret/ledger.db",
            "ledgerPath",
            "socketPath",
            "transcript",
        ] {
            assert!(!serialized.contains(forbidden), "found {forbidden}");
        }
    }

    #[test]
    fn missing_typed_capability_stops_before_session_or_transcript_reads() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let workspace_id = paths::workspace_id(workspace.path()).unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let hello = read_request(&mut reader);
            write_response(
                &mut writer,
                hello.id,
                "hello",
                json!({
                    "daemon_version": "old",
                    "workspace_id": workspace_id,
                    "ledger_path": "/tmp/ledger.db",
                    "capabilities": ["hello", "sessions.list", "transcript.read"]
                }),
            );
        });

        let error = connect_workspace(workspace.path(), Some(socket_path)).unwrap_err();
        handle.join().unwrap();

        assert_eq!(
            error,
            "Incompatible daemon: missing required capability transcript.read.typed"
        );
    }

    #[test]
    fn hello_validation_rejects_a_different_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let hello = HelloResult {
            daemon_version: "0.1.0".into(),
            workspace_id: "other-workspace".into(),
            ledger_path: "/secret/ledger.db".into(),
            capabilities: REQUIRED_CAPABILITIES
                .iter()
                .map(ToString::to_string)
                .collect(),
        };

        let error = validate_hello(workspace.path(), &hello).unwrap_err();

        assert!(error.starts_with("Incompatible daemon: expected workspace "));
        assert!(error.ends_with(", got other-workspace"));
        assert!(!error.contains("ledger.db"));
    }

    #[test]
    fn exact_run_typed_payload_fails_closed_on_missing_or_wrong_boundaries() {
        let base = TranscriptReadResult {
            run_id: "run_1".into(),
            status: RunStateName::Finished,
            final_answer: Some("answer".into()),
            transcript: "legacy".into(),
            typed: None,
            pending_approval: None,
        };
        assert_eq!(
            extract_typed_run("run_1", base.clone()).unwrap_err(),
            "Incompatible daemon: transcript.read returned no typed payload"
        );

        let mut multiple = base.clone();
        multiple.typed = Some(plato_agent::daemon::protocol::TypedTranscript {
            runs: vec![typed_run("run_1"), typed_run("run_2")],
        });
        assert_eq!(
            extract_typed_run("run_1", multiple).unwrap_err(),
            "Incompatible daemon: exact-run transcript returned 2 runs"
        );

        let mut wrong = base;
        wrong.typed = Some(plato_agent::daemon::protocol::TypedTranscript {
            runs: vec![typed_run("run_2")],
        });
        assert_eq!(
            extract_typed_run("run_1", wrong).unwrap_err(),
            "Incompatible daemon: requested run run_1, got run_2"
        );
    }

    fn typed_run(run_id: &str) -> TypedRun {
        TypedRun {
            run_id: run_id.into(),
            session_index: 0,
            status: RunStateName::Finished,
            entries: vec![],
        }
    }

    fn read_request(reader: &mut BufReader<UnixStream>) -> Envelope {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let envelope: Envelope = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(envelope.kind, EnvelopeKind::Request);
        envelope
    }

    fn write_response(writer: &mut UnixStream, id: Option<String>, method: &str, result: Value) {
        let response = Envelope {
            v: PROTOCOL_VERSION,
            id,
            kind: EnvelopeKind::Response,
            method: Some(method.into()),
            params: None,
            result: Some(result),
            error: None,
        };
        serde_json::to_writer(writer.by_ref(), &response).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
    }
}
