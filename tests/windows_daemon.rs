#![cfg(windows)]
#![allow(unsafe_code)]

use interprocess::local_socket::{GenericFilePath, Stream, prelude::*};
use plato_agent::{
    daemon::{
        client::DaemonClient,
        protocol::{RunStateName, ShutdownIfIdleResultName},
        server::DaemonServer,
    },
    paths,
};
use serde_json::json;
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    ptr,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{
        ImpersonateLoggedOnUser, LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT, LogonUserW,
        RevertToSelf,
    },
    System::{
        Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent},
        Threading::CREATE_NEW_PROCESS_GROUP,
    },
};

const PROOF_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn daemon_round_trip_streams_and_replays_after_clean_shutdown() {
    let provider = FakeProvider::start("Windows reply");
    let workspace = tempfile::tempdir().unwrap();
    let config_path = workspace.path().join("plato.toml");
    write_provider_config(&config_path, &provider.base_url);

    let server = DaemonServer::bind(workspace.path(), None).unwrap();
    let paths = server.paths().clone();
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn(move || server.serve_forever(shutdown));

    let mut client = connect_bounded(&paths.socket_path);
    let hello = client.hello(workspace.path()).unwrap();
    assert_eq!(hello.workspace_id, paths.workspace_id);
    assert_eq!(hello.ledger_path, paths.ledger_path.to_string_lossy());

    let started = client
        .run_start(
            "prove Windows transport".into(),
            Some(config_path.to_string_lossy().into_owned()),
            false,
        )
        .unwrap();
    assert_eq!(started.status, RunStateName::Running);

    let deadline = Instant::now() + PROOF_TIMEOUT;
    let mut offset = None;
    let mut saw_delta = false;
    loop {
        let page = client.events_stream(&started.run_id, offset, 128).unwrap();
        assert_eq!(page.run_id, started.run_id);
        saw_delta |= page.events.iter().any(|entry| {
            entry["event"]["kind"] == "assistant_delta"
                && entry["event"]["run_id"] == started.run_id
                && entry["event"]["text"] == "Windows reply"
        });
        offset = Some(page.next_offset);
        if page.status == RunStateName::Finished {
            break;
        }
        assert!(Instant::now() < deadline, "Windows run did not finish");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(saw_delta, "live exact-run delta was not observed");

    let transcript = client.transcript_read(&started.run_id).unwrap();
    assert_eq!(transcript.run_id, started.run_id);
    assert_eq!(transcript.status, RunStateName::Finished);
    assert_eq!(transcript.final_answer.as_deref(), Some("Windows reply"));
    assert_eq!(transcript.typed.unwrap().runs[0].run_id, started.run_id);

    let result = client.shutdown_if_idle().unwrap();
    assert_eq!(result.result, ShutdownIfIdleResultName::Shutdown);
    drop(client);
    handle.join().unwrap().unwrap();
    provider.join();

    assert!(!paths.lock_path.exists());
    assert!(DaemonClient::connect(&paths.socket_path).is_err());

    let replay = Command::new(env!("CARGO_BIN_EXE_plato"))
        .current_dir(workspace.path())
        .arg("replay")
        .arg(format!("--db={}", paths.ledger_path.display()))
        .arg("--run")
        .arg(&started.run_id)
        .output()
        .unwrap();
    assert!(
        replay.status.success(),
        "offline replay failed: {}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert!(String::from_utf8_lossy(&replay.stdout).contains("Windows reply"));
}

#[test]
fn ctrl_break_stops_daemon_and_removes_lock() {
    let workspace = tempfile::tempdir().unwrap();
    let lock_path = paths::default_lock_path(workspace.path()).unwrap();
    let socket_path = paths::default_socket_path(workspace.path()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_plato-agentd"))
        .arg("--workspace")
        .arg(workspace.path())
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&lock_path, &mut child);
    assert_ne!(
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) },
        0
    );
    let status = wait_bounded(&mut child, PROOF_TIMEOUT).unwrap();
    assert!(status.success());
    assert!(!lock_path.exists());
    assert!(DaemonClient::connect(&socket_path).is_err());
}

#[test]
#[ignore = "requires PLATO_WINDOWS_SECOND_USER and PLATO_WINDOWS_SECOND_PASSWORD"]
fn production_pipe_and_lock_reject_second_user_and_remote_clients() {
    let username = env::var("PLATO_WINDOWS_SECOND_USER").unwrap();
    let password = env::var("PLATO_WINDOWS_SECOND_PASSWORD").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let server = DaemonServer::bind(workspace.path(), None).unwrap();
    let paths = server.paths().clone();
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn(move || server.serve_forever(shutdown));

    assert!(File::open(&paths.lock_path).is_ok());
    {
        let _impersonation = Impersonation::start(&username, &password).unwrap();
        assert_access_denied(Stream::connect(
            paths
                .socket_path
                .as_os_str()
                .to_fs_name::<GenericFilePath>()
                .unwrap(),
        ));
        assert_access_denied(File::open(&paths.lock_path));
        assert_access_denied(OpenOptions::new().write(true).open(&paths.lock_path));
        assert_access_denied(fs::remove_file(&paths.lock_path));
    }

    prove_remote_rejection(&paths.socket_path);

    let mut client = connect_bounded(&paths.socket_path);
    client.hello(workspace.path()).unwrap();
    assert_eq!(
        client.shutdown_if_idle().unwrap().result,
        ShutdownIfIdleResultName::Shutdown
    );
    drop(client);
    handle.join().unwrap().unwrap();
    assert!(!paths.lock_path.exists());
}

#[test]
#[ignore = "child process for the production remote-rejection proof"]
fn remote_pipe_probe_child() {
    let path = env::var_os("PLATO_WINDOWS_REMOTE_PIPE").unwrap();
    assert_access_denied(Stream::connect(
        path.as_os_str().to_fs_name::<GenericFilePath>().unwrap(),
    ));
}

fn prove_remote_rejection(local_path: &Path) {
    let prefix = r"\\.\pipe\";
    let local = local_path.to_string_lossy();
    let name = local
        .strip_prefix(prefix)
        .expect("production endpoint must be a local named pipe");
    let computer = env::var("COMPUTERNAME").unwrap();
    let remote = format!(r"\\{computer}\pipe\{name}");
    let mut child = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "remote_pipe_probe_child",
            "--ignored",
            "--nocapture",
        ])
        .env("PLATO_WINDOWS_REMOTE_PIPE", remote)
        .spawn()
        .unwrap();
    let status = wait_bounded(&mut child, Duration::from_secs(10)).unwrap();
    assert!(status.success(), "remote named-pipe probe failed closed");
}

fn assert_access_denied<T>(result: io::Result<T>) {
    match result {
        Err(error) if error.raw_os_error() == Some(5) => {}
        Err(error) => panic!("expected ERROR_ACCESS_DENIED, got {error}"),
        Ok(_) => panic!("unauthorized Windows operation succeeded"),
    }
}

fn connect_bounded(path: &Path) -> DaemonClient {
    let deadline = Instant::now() + PROOF_TIMEOUT;
    loop {
        match DaemonClient::connect(path) {
            Ok(client) => return client,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "daemon did not accept clients: {error}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn wait_for_path(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + PROOF_TIMEOUT;
    loop {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("daemon exited before creating its lock ({status}): {stderr}");
        }
        assert!(Instant::now() < deadline, "daemon did not create its lock");
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_bounded(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "child timed out"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

struct FakeProvider {
    base_url: String,
    handle: thread::JoinHandle<()>,
}

impl FakeProvider {
    fn start(answer: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream);
            let content = json!({
                "choices": [{
                    "index": 0,
                    "delta": {"content": answer},
                    "finish_reason": null
                }]
            });
            let finish = json!({
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            });
            let body = format!("data: {content}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        Self { base_url, handle }
    }

    fn join(self) {
        self.handle.join().unwrap();
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) {
    stream.set_read_timeout(Some(PROOF_TIMEOUT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
}

fn write_provider_config(path: &Path, base_url: &str) {
    fs::write(
        path,
        format!(
            r#"[provider]
kind = "open_ai"
model = "test-model"
api_key_env = "PATH"
base_url = "{base_url}"
timeout_ms = 15000

[limits]
token_budget = 4000
max_output_tokens = 32
max_turns = 1

[tools]
enabled = ["file.read"]
"#
        ),
    )
    .unwrap();
}

struct Impersonation {
    token: HANDLE,
}

impl Impersonation {
    fn start(username: &str, password: &str) -> io::Result<Self> {
        let username = wide(username);
        let domain = wide(".");
        let password = wide(password);
        let mut token: HANDLE = ptr::null_mut();
        if unsafe {
            LogonUserW(
                username.as_ptr(),
                domain.as_ptr(),
                password.as_ptr(),
                LOGON32_LOGON_NETWORK,
                LOGON32_PROVIDER_DEFAULT,
                &mut token,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { ImpersonateLoggedOnUser(token) } == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(token);
            }
            return Err(error);
        }
        Ok(Self { token })
    }
}

impl Drop for Impersonation {
    fn drop(&mut self) {
        assert_ne!(unsafe { RevertToSelf() }, 0);
        unsafe {
            CloseHandle(self.token);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}
