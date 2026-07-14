#![cfg(windows)]
#![allow(unsafe_code)]

use interprocess::{
    local_socket::{GenericFilePath, ListenerOptions, Stream, prelude::*},
    os::windows::{local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor},
};
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
use widestring::U16CString;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Security::{
        GetTokenInformation, ImpersonateLoggedOnUser, LOGON32_LOGON_NETWORK,
        LOGON32_PROVIDER_DEFAULT, LogonUserW, RevertToSelf, SECURITY_IMPERSONATION_LEVEL,
        SecurityIdentification, TOKEN_QUERY, TokenImpersonationLevel,
    },
    System::{
        Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, FreeConsole,
            GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
        },
        Threading::{
            CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
            CreateProcessWithLogonW, GetCurrentThread, GetExitCodeProcess, OpenThreadToken,
            PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
        },
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
    let mut offset = Some(0);
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
    let mut client = connect_bounded(&socket_path);
    client.hello(workspace.path()).unwrap();
    drop(client);
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
#[ignore = "temporarily reattaches the test process to the daemon console"]
fn ctrl_c_stops_daemon_and_removes_lock() {
    let workspace = tempfile::tempdir().unwrap();
    let lock_path = paths::default_lock_path(workspace.path()).unwrap();
    let socket_path = paths::default_socket_path(workspace.path()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_plato-agentd"))
        .arg("--workspace")
        .arg(workspace.path())
        .creation_flags(CREATE_NEW_CONSOLE)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&lock_path, &mut child);
    let mut client = connect_bounded(&socket_path);
    client.hello(workspace.path()).unwrap();
    drop(client);
    {
        let _console = ChildConsole::attach(child.id()).unwrap();
        assert_ne!(unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) }, 0);
        thread::sleep(Duration::from_millis(100));
    }
    let status = wait_bounded(&mut child, PROOF_TIMEOUT).unwrap();
    assert!(status.success());
    assert!(!lock_path.exists());
    assert!(DaemonClient::connect(&socket_path).is_err());
}

#[test]
fn long_workspace_name_binds_and_removes_its_lock() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("w".repeat(200));
    fs::create_dir(&workspace).unwrap();
    let server = DaemonServer::bind(&workspace, None).unwrap();
    let paths = server.paths().clone();
    assert!(paths.workspace_id.len() <= 81);
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn(move || server.serve_forever(shutdown));
    let mut client = connect_bounded(&paths.socket_path);
    client.hello(&workspace).unwrap();
    assert_eq!(
        client.shutdown_if_idle().unwrap().result,
        ShutdownIfIdleResultName::Shutdown
    );
    drop(client);
    handle.join().unwrap().unwrap();
    assert!(!paths.lock_path.exists());
}

#[test]
fn client_pipe_limits_server_impersonation_to_identification() {
    let workspace = tempfile::tempdir().unwrap();
    let endpoint = paths::default_socket_path(workspace.path()).unwrap();
    let listener = hostile_listener(&endpoint);
    let workspace_id = paths::workspace_id(workspace.path()).unwrap();
    let server = thread::spawn(move || {
        let stream = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(&stream).read_line(&mut request).unwrap();
        let request: serde_json::Value = serde_json::from_str(request.trim()).unwrap();
        let Stream::NamedPipe(stream) = stream;
        let _impersonation = stream.inner().impersonate_client().unwrap();
        assert_eq!(current_thread_impersonation_level(), SecurityIdentification);
        let response = json!({
            "v": 1,
            "id": request["id"],
            "kind": "response",
            "method": "hello",
            "result": {
                "daemon_version": "test",
                "workspace_id": workspace_id,
                "ledger_path": "test",
                "capabilities": []
            }
        });
        writeln!(&stream, "{response}").unwrap();
    });

    let mut client = DaemonClient::connect(&endpoint).unwrap();
    client.hello(workspace.path()).unwrap();
    server.join().unwrap();
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
#[ignore = "requires PLATO_WINDOWS_SECOND_USER and PLATO_WINDOWS_SECOND_PASSWORD"]
fn client_rejects_second_user_prebound_pipes() {
    let username = env::var("PLATO_WINDOWS_SECOND_USER").unwrap();
    let password = env::var("PLATO_WINDOWS_SECOND_PASSWORD").unwrap();
    let public = env::var_os("PUBLIC").expect("PUBLIC is required for the cross-user proof");
    let shared = tempfile::Builder::new()
        .prefix("plato-147-")
        .tempdir_in(public)
        .unwrap();
    let grant = Command::new("icacls.exe")
        .arg(shared.path())
        .args(["/grant", &format!("{username}:(OI)(CI)M")])
        .output()
        .unwrap();
    assert!(
        grant.status.success(),
        "failed to grant helper directory access: {}",
        String::from_utf8_lossy(&grant.stderr)
    );
    let helper = shared.path().join("plato-windows-pipe-helper.exe");
    fs::copy(env::current_exe().unwrap(), &helper).unwrap();

    let available_ready = shared.path().join("hostile-available-ready");
    let mut available = LoggedOnProcess::spawn_test(
        &username,
        &password,
        &helper,
        shared.path(),
        "hostile_available_pipe_child",
    )
    .unwrap();
    wait_for_marker(&available_ready, &mut available);

    let endpoint = paths::default_socket_path(shared.path()).unwrap();
    let lock_path = paths::default_lock_path(shared.path()).unwrap();
    assert!(
        DaemonServer::bind(shared.path(), None).is_err(),
        "the legitimate daemon replaced a hostile first pipe instance"
    );
    assert!(
        !lock_path.exists(),
        "failed bind left a workspace lock behind"
    );
    let error = match DaemonClient::connect(&endpoint) {
        Ok(_) => panic!("client accepted a second-user pipe server"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("named-pipe server is not owned by the current user"),
        "unexpected hostile-server error: {error}"
    );
    assert_eq!(available.wait_bounded(PROOF_TIMEOUT).unwrap(), 0);

    let busy_ready = shared.path().join("hostile-busy-ready");
    let mut busy = LoggedOnProcess::spawn_test(
        &username,
        &password,
        &helper,
        shared.path(),
        "hostile_busy_pipe_child",
    )
    .unwrap();
    wait_for_marker(&busy_ready, &mut busy);
    let started = Instant::now();
    let error = match DaemonClient::connect(&endpoint) {
        Ok(_) => panic!("client connected to a busy second-user pipe server"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("named-pipe connection timed out"),
        "unexpected busy hostile-server error: {error}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(busy.wait_bounded(PROOF_TIMEOUT).unwrap(), 0);
}

#[test]
#[ignore = "child process for the production remote-rejection proof"]
fn remote_pipe_probe_child() {
    let path = env::var_os("PLATO_WINDOWS_REMOTE_PIPE").unwrap();
    assert_access_denied(Stream::connect(
        path.as_os_str().to_fs_name::<GenericFilePath>().unwrap(),
    ));
}

#[test]
#[ignore = "second-user child for the hostile available-pipe proof"]
fn hostile_available_pipe_child() {
    let endpoint = paths::default_socket_path(&env::current_dir().unwrap()).unwrap();
    let listener = hostile_listener(&endpoint);
    fs::write("hostile-available-ready", b"ready").unwrap();
    let stream = listener.accept().unwrap();
    let mut byte = [0; 1];
    assert_eq!((&stream).read(&mut byte).unwrap(), 0);
}

#[test]
#[ignore = "second-user child for the hostile busy-pipe proof"]
fn hostile_busy_pipe_child() {
    let endpoint = paths::default_socket_path(&env::current_dir().unwrap()).unwrap();
    let listener = hostile_listener(&endpoint);
    let endpoint_for_client = endpoint.clone();
    let (connected_tx, connected_rx) = std::sync::mpsc::sync_channel(0);
    let client = thread::spawn(move || {
        let stream = Stream::connect(
            endpoint_for_client
                .as_os_str()
                .to_fs_name::<GenericFilePath>()
                .unwrap(),
        )
        .unwrap();
        connected_tx.send(()).unwrap();
        thread::sleep(Duration::from_secs(3));
        drop(stream);
    });
    connected_rx.recv_timeout(PROOF_TIMEOUT).unwrap();
    fs::write("hostile-busy-ready", b"ready").unwrap();
    thread::sleep(Duration::from_secs(3));
    drop(listener);
    client.join().unwrap();
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

fn hostile_listener(path: &Path) -> interprocess::local_socket::Listener {
    let descriptor = U16CString::from_str("D:P(A;;GA;;;WD)").unwrap();
    let descriptor = SecurityDescriptor::deserialize(&descriptor).unwrap();
    ListenerOptions::new()
        .name(path.as_os_str().to_fs_name::<GenericFilePath>().unwrap())
        .security_descriptor(descriptor)
        .create_sync()
        .unwrap()
}

fn wait_for_marker(path: &Path, child: &mut LoggedOnProcess) {
    let deadline = Instant::now() + PROOF_TIMEOUT;
    loop {
        if path.exists() {
            return;
        }
        if let Some(code) = child.try_wait().unwrap() {
            panic!("hostile pipe helper exited before ready with code {code}");
        }
        assert!(
            Instant::now() < deadline,
            "hostile pipe helper did not become ready"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

struct LoggedOnProcess {
    process: HANDLE,
}

impl LoggedOnProcess {
    fn spawn_test(
        username: &str,
        password: &str,
        executable: &Path,
        cwd: &Path,
        test_name: &str,
    ) -> io::Result<Self> {
        let username = wide(username);
        let domain = wide(".");
        let password = wide(password);
        let executable_wide = wide_os(executable.as_os_str());
        let cwd = wide_os(cwd.as_os_str());
        let command_line = format!(
            "\"{}\" --exact {test_name} --ignored --nocapture",
            executable.display()
        );
        let mut command_line = wide(&command_line);
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>()
                .try_into()
                .expect("STARTUPINFOW size fits u32"),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: every string is NUL-terminated, command_line is writable, and both output
        // structures remain live for the call.
        if unsafe {
            CreateProcessWithLogonW(
                username.as_ptr(),
                domain.as_ptr(),
                password.as_ptr(),
                0,
                executable_wide.as_ptr(),
                command_line.as_mut_ptr(),
                CREATE_NO_WINDOW,
                ptr::null(),
                cwd.as_ptr(),
                &startup,
                &mut process,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the thread handle is independent; the process handle remains owned below.
        unsafe {
            CloseHandle(process.hThread);
        }
        Ok(Self {
            process: process.hProcess,
        })
    }

    fn try_wait(&mut self) -> io::Result<Option<u32>> {
        // SAFETY: process is a live process handle.
        match unsafe { WaitForSingleObject(self.process, 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: process is signaled and code is writable output storage.
                if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(Some(code))
                }
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn wait_bounded(&mut self, timeout: Duration) -> io::Result<u32> {
        let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
        // SAFETY: process is a live process handle.
        match unsafe { WaitForSingleObject(self.process, timeout_ms) } {
            WAIT_OBJECT_0 => self
                .try_wait()?
                .ok_or_else(|| io::Error::other("signaled process had no exit code")),
            WAIT_TIMEOUT => {
                // SAFETY: process is live and was created with termination rights.
                unsafe {
                    TerminateProcess(self.process, 1);
                    WaitForSingleObject(self.process, 5_000);
                }
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "hostile pipe helper timed out",
                ))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
}

impl Drop for LoggedOnProcess {
    fn drop(&mut self) {
        // SAFETY: process is owned here; a still-running helper is terminated before close.
        unsafe {
            if WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT {
                TerminateProcess(self.process, 1);
                WaitForSingleObject(self.process, 5_000);
            }
            CloseHandle(self.process);
        }
    }
}

struct ChildConsole;

impl ChildConsole {
    fn attach(process_id: u32) -> io::Result<Self> {
        // SAFETY: detaching only changes this test process's console association.
        unsafe {
            FreeConsole();
        }
        // SAFETY: process_id names the live daemon whose dedicated console is the proof target.
        if unsafe { AttachConsole(process_id) } == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: best-effort restoration of the test process's parent console.
            unsafe {
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
            return Err(error);
        }
        // SAFETY: this process must ignore the broadcast Ctrl-C that it generates.
        if unsafe { SetConsoleCtrlHandler(None, 1) } == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: restore the original console association before returning.
            unsafe {
                FreeConsole();
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for ChildConsole {
    fn drop(&mut self) {
        // SAFETY: restore this process's normal Ctrl-C handling and parent console.
        unsafe {
            FreeConsole();
            SetConsoleCtrlHandler(None, 0);
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn current_thread_impersonation_level() -> SECURITY_IMPERSONATION_LEVEL {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: token is writable and GetCurrentThread returns the current thread pseudo-handle.
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
        panic!(
            "failed to open named-pipe impersonation token: {}",
            io::Error::last_os_error()
        );
    }
    let mut level = 0;
    let mut bytes = 0;
    // SAFETY: token is live and level is correctly sized writable output storage.
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenImpersonationLevel,
            (&mut level as *mut SECURITY_IMPERSONATION_LEVEL).cast(),
            std::mem::size_of::<SECURITY_IMPERSONATION_LEVEL>() as u32,
            &mut bytes,
        )
    };
    let error = (result == 0).then(io::Error::last_os_error);
    // SAFETY: token was returned by OpenThreadToken and is owned here.
    unsafe {
        CloseHandle(token);
    }
    assert!(
        error.is_none(),
        "failed to read impersonation level: {}",
        error.unwrap()
    );
    level
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

fn wide_os(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
