use crate::{
    AppError, AppResult,
    config::{Config, TelegramGatewayConfig, resolve_config_path},
    daemon::{
        client::{DaemonClient, DaemonConnectionConfig},
        protocol::{HelloResult, TranscriptReadResult},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::PathBuf,
    thread,
    time::Duration,
};

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const TELEGRAM_LONG_POLL_SECONDS: u64 = 30;
const EVENT_PAGE_LIMIT: usize = 64;
const EVENT_POLL_DELAY: Duration = Duration::from_millis(100);
const RECONNECT_ATTEMPTS: usize = 40;
const RECONNECT_DELAY: Duration = Duration::from_millis(50);
const REQUIRED_CAPABILITIES: [&str; 5] = [
    "hello",
    "message.append",
    "events.stream",
    "sessions.list",
    "transcript.read",
];

pub struct TelegramGatewayOptions {
    pub workspace_root: PathBuf,
    pub socket_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
}

pub fn run_telegram_gateway(options: TelegramGatewayOptions) -> AppResult<()> {
    let config_path = resolve_config_path(&options.workspace_root, options.config_path.as_deref())?;
    let config = Config::load(&options.workspace_root, config_path.as_deref())?;
    let telegram = config
        .gateway
        .clone()
        .map(|gateway| gateway.telegram)
        .ok_or_else(|| AppError::Config("gateway.telegram configuration is required".into()))?;
    let token = gateway_token(&config, &telegram, |name| std::env::var_os(name))?;
    let daemon = DaemonConnectionConfig::resolve(&options.workspace_root, options.socket_path)?;
    let platform = TelegramClient::new(TELEGRAM_API_BASE, token, TELEGRAM_LONG_POLL_SECONDS);
    let config_path = config_path.map(|path| path.to_string_lossy().into_owned());
    TelegramGateway::new(platform, daemon, config_path, telegram).run()
}

fn gateway_token(
    config: &Config,
    telegram: &TelegramGatewayConfig,
    env: impl Fn(&str) -> Option<OsString>,
) -> AppResult<String> {
    let provider_envs = [
        config.provider.api_key_env.as_str(),
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
    ];
    if provider_envs
        .iter()
        .any(|name| *name == telegram.api_key_env)
    {
        return Err(AppError::Config(
            "gateway token env var must differ from provider credential env vars".into(),
        ));
    }
    for name in provider_envs {
        if env(name).is_some() {
            return Err(AppError::Config(format!(
                "gateway refuses provider credential env var {name}"
            )));
        }
    }
    let token = env(&telegram.api_key_env).ok_or_else(|| {
        AppError::Config(format!(
            "gateway token env var {} is not set",
            telegram.api_key_env
        ))
    })?;
    let token = token
        .into_string()
        .map_err(|_| AppError::Config("gateway token is not valid UTF-8".into()))?;
    if token.is_empty() {
        return Err(AppError::Config("gateway token must not be empty".into()));
    }
    Ok(token)
}

struct TelegramGateway {
    platform: TelegramClient,
    daemon: DaemonConnectionConfig,
    config_path: Option<String>,
    owner_user_ids: HashSet<i64>,
    sessions: HashMap<Conversation, String>,
    next_update_id: Option<i64>,
    event_poll_delay: Duration,
    reconnect_delay: Duration,
}

impl TelegramGateway {
    fn new(
        platform: TelegramClient,
        daemon: DaemonConnectionConfig,
        config_path: Option<String>,
        config: TelegramGatewayConfig,
    ) -> Self {
        Self {
            platform,
            daemon,
            config_path,
            owner_user_ids: config.owner_user_ids.into_iter().collect(),
            sessions: HashMap::new(),
            next_update_id: None,
            event_poll_delay: EVENT_POLL_DELAY,
            reconnect_delay: RECONNECT_DELAY,
        }
    }

    fn run(mut self) -> AppResult<()> {
        loop {
            self.poll_once()?;
        }
    }

    fn poll_once(&mut self) -> AppResult<()> {
        let updates = self.platform.get_updates(self.next_update_id)?;
        for update in updates {
            self.next_update_id = Some(
                self.next_update_id
                    .unwrap_or_default()
                    .max(update.update_id.saturating_add(1)),
            );
            let Some(message) = update.message else {
                continue;
            };
            let Some(sender) = message.sender else {
                continue;
            };
            if !self.owner_user_ids.contains(&sender.id) {
                continue;
            }
            let Some(text) = message.text.filter(|text| !text.trim().is_empty()) else {
                continue;
            };
            let conversation = Conversation {
                chat_id: message.chat.id,
                thread_id: message.thread_id,
            };
            self.handle_message(conversation, text)?;
        }
        Ok(())
    }

    fn handle_message(&mut self, conversation: Conversation, text: String) -> AppResult<()> {
        let mut daemon = self.connect_daemon()?;
        let session_id = self.sessions.get(&conversation).cloned();
        let run =
            daemon.message_append_to_session(text, session_id, self.config_path.clone(), false)?;
        self.sessions
            .insert(conversation.clone(), run.session_id.clone());
        let answer = self.wait_for_run(&mut daemon, &run.run_id, &run.session_id)?;
        self.platform.send_message(&conversation, &answer)
    }

    fn wait_for_run(
        &self,
        daemon: &mut DaemonClient,
        run_id: &str,
        session_id: &str,
    ) -> AppResult<String> {
        let mut next_offset = Some(0);
        loop {
            match daemon.events_stream(run_id, next_offset, EVENT_PAGE_LIMIT) {
                Ok(events) => {
                    next_offset = Some(events.next_offset);
                    if events.status != "running" {
                        return self.read_terminal_answer(daemon, session_id);
                    }
                    if events.events.is_empty() {
                        thread::sleep(self.event_poll_delay);
                    }
                }
                Err(AppError::DaemonResponse(error)) if error.code == "lagged" => {
                    next_offset = None;
                }
                Err(error) if reconnectable(&error) => {
                    *daemon = self.reconnect_daemon()?;
                    let session = daemon
                        .sessions_list()?
                        .into_iter()
                        .find(|session| session.session_id == session_id)
                        .ok_or_else(|| AppError::SessionNotFound(session_id.into()))?;
                    if session.status != "running" {
                        return self.read_terminal_answer(daemon, session_id);
                    }
                    next_offset = None;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn reconnect_daemon(&self) -> AppResult<DaemonClient> {
        for _ in 0..RECONNECT_ATTEMPTS {
            match self.connect_daemon() {
                Ok(client) => return Ok(client),
                Err(error) if reconnectable(&error) => thread::sleep(self.reconnect_delay),
                Err(error) => return Err(error),
            }
        }
        Err(AppError::DaemonProtocol(
            "daemon unavailable during gateway recovery".into(),
        ))
    }

    fn connect_daemon(&self) -> AppResult<DaemonClient> {
        let mut client = DaemonClient::connect(&self.daemon.socket_path)?;
        let hello = client.hello(&self.daemon.workspace_root)?;
        require_capabilities(&hello)?;
        Ok(client)
    }

    fn read_terminal_answer(
        &self,
        daemon: &mut DaemonClient,
        session_id: &str,
    ) -> AppResult<String> {
        match daemon.transcript_read_session(session_id) {
            Ok(transcript) => terminal_answer(transcript),
            Err(error) if reconnectable(&error) => {
                *daemon = self.reconnect_daemon()?;
                terminal_answer(daemon.transcript_read_session(session_id)?)
            }
            Err(error) => Err(error),
        }
    }
}

fn require_capabilities(hello: &HelloResult) -> AppResult<()> {
    if let Some(capability) = REQUIRED_CAPABILITIES.iter().find(|capability| {
        !hello
            .capabilities
            .iter()
            .any(|actual| actual == **capability)
    }) {
        return Err(AppError::DaemonProtocol(format!(
            "daemon does not advertise required capability {capability}"
        )));
    }
    Ok(())
}

fn terminal_answer(transcript: TranscriptReadResult) -> AppResult<String> {
    if let Some(answer) = transcript.final_answer {
        return Ok(answer);
    }
    Err(AppError::RunFailed(format!(
        "run {} ended with status {} without a final answer",
        transcript.run_id, transcript.status
    )))
}

fn reconnectable(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Io(_) | AppError::Json(_) | AppError::DaemonProtocol(_)
    ) || matches!(
        error,
        AppError::DaemonResponse(error) if error.code == "not_found"
    )
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Conversation {
    chat_id: i64,
    thread_id: Option<i64>,
}

struct TelegramClient {
    agent: ureq::Agent,
    base_url: String,
    token: String,
    long_poll_seconds: u64,
}

impl TelegramClient {
    fn new(base_url: &str, token: String, long_poll_seconds: u64) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_read(Duration::from_secs(long_poll_seconds + 5))
                .build(),
            base_url: base_url.trim_end_matches('/').into(),
            token,
            long_poll_seconds,
        }
    }

    fn get_updates(&self, offset: Option<i64>) -> AppResult<Vec<TelegramUpdate>> {
        let endpoint = self.endpoint("getUpdates");
        let mut request = self
            .agent
            .get(&endpoint)
            .query("timeout", &self.long_poll_seconds.to_string())
            .query("allowed_updates", r#"["message"]"#);
        if let Some(offset) = offset {
            request = request.query("offset", &offset.to_string());
        }
        let response = request
            .call()
            .map_err(|error| telegram_request_error("getUpdates", error))?;
        let response: TelegramResponse<Vec<TelegramUpdate>> = response
            .into_json()
            .map_err(|_| AppError::Provider("telegram getUpdates returned invalid JSON".into()))?;
        if !response.ok {
            return Err(AppError::Provider(
                "telegram getUpdates was rejected".into(),
            ));
        }
        response
            .result
            .ok_or_else(|| AppError::Provider("telegram getUpdates response missing result".into()))
    }

    fn send_message(&self, conversation: &Conversation, text: &str) -> AppResult<()> {
        for text in telegram_chunks(text) {
            let response = self
                .agent
                .post(&self.endpoint("sendMessage"))
                .send_json(SendMessage {
                    chat_id: conversation.chat_id,
                    message_thread_id: conversation.thread_id,
                    text,
                })
                .map_err(|error| telegram_request_error("sendMessage", error))?;
            let response: TelegramResponse<serde_json::Value> =
                response.into_json().map_err(|_| {
                    AppError::Provider("telegram sendMessage returned invalid JSON".into())
                })?;
            if !response.ok {
                return Err(AppError::Provider(
                    "telegram sendMessage was rejected".into(),
                ));
            }
            if response.result.is_none() {
                return Err(AppError::Provider(
                    "telegram sendMessage response missing result".into(),
                ));
            }
        }
        Ok(())
    }

    fn endpoint(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.base_url, self.token)
    }
}

fn telegram_chunks(text: &str) -> Vec<String> {
    let characters = text.chars().collect::<Vec<_>>();
    characters
        .chunks(4096)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn telegram_request_error(operation: &str, error: ureq::Error) -> AppError {
    match error {
        ureq::Error::Status(status, _) => {
            AppError::Provider(format!("telegram {operation} returned HTTP {status}"))
        }
        ureq::Error::Transport(_) => {
            AppError::Provider(format!("telegram {operation} transport failed"))
        }
    }
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    #[serde(rename = "from")]
    sender: Option<TelegramUser>,
    chat: TelegramChat,
    #[serde(rename = "message_thread_id")]
    thread_id: Option<i64>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Serialize)]
struct SendMessage {
    chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{Envelope, EnvelopeKind, PROTOCOL_VERSION};
    use serde_json::{Value, json};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        os::unix::net::{UnixListener, UnixStream},
        path::Path,
        thread,
        time::Instant,
    };

    #[test]
    fn gateway_environment_rejects_provider_credentials() {
        let config = Config::default();
        let telegram = telegram_config();

        let error = gateway_token(&config, &telegram, |name| match name {
            "TELEGRAM_BOT_TOKEN" => Some(OsString::from("telegram-secret")),
            "OPENROUTER_API_KEY" => Some(OsString::from("provider-secret")),
            _ => None,
        })
        .unwrap_err();

        assert!(error.to_string().contains("OPENROUTER_API_KEY"));
        assert!(!error.to_string().contains("provider-secret"));
        assert!(!error.to_string().contains("telegram-secret"));
    }

    #[test]
    fn non_owner_updates_are_silently_ignored() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("missing.sock");
        let platform = spawn_fake_telegram(update(7, 99, 200, None, "ignore me"), 0);
        let mut gateway = test_gateway(
            &workspace,
            socket_path,
            &platform.base_url,
            telegram_config(),
        );

        gateway.poll_once().unwrap();

        let requests = platform.handle.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(gateway.sessions.is_empty());
    }

    #[test]
    fn owner_message_replies_with_typed_final_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("daemon.sock");
        let daemon = spawn_finished_daemon(&socket_path, "final answer");
        let platform = spawn_fake_telegram(update(7, 42, 200, Some(9), "hello"), 1);
        let mut gateway = test_gateway(
            &workspace,
            socket_path,
            &platform.base_url,
            telegram_config(),
        );

        gateway.poll_once().unwrap();

        let append_params = daemon.join().unwrap();
        assert_eq!(append_params["message"], "hello");
        assert!(append_params["session_id"].is_null());
        assert_eq!(append_params["wait"], false);
        let requests = platform.handle.join().unwrap();
        assert_eq!(requests[1].body["chat_id"], 200);
        assert_eq!(requests[1].body["message_thread_id"], 9);
        assert_eq!(requests[1].body["text"], "final answer");
        assert_eq!(
            gateway.sessions[&Conversation {
                chat_id: 200,
                thread_id: Some(9),
            }],
            "session_1"
        );
    }

    #[test]
    fn daemon_restart_recovers_final_answer_from_typed_transcript() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("daemon.sock");
        let daemon = spawn_restarting_daemon(&socket_path, "recovered answer");
        let platform = spawn_fake_telegram(update(7, 42, 200, None, "hello"), 1);
        let mut gateway = test_gateway(
            &workspace,
            socket_path,
            &platform.base_url,
            telegram_config(),
        );

        gateway.poll_once().unwrap();

        daemon.join().unwrap();
        let requests = platform.handle.join().unwrap();
        assert_eq!(requests[1].body["text"], "recovered answer");
    }

    #[test]
    fn lag_resumes_at_tip_and_reads_typed_final_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("daemon.sock");
        let daemon = spawn_lagged_daemon(&socket_path, "answer after lag");
        let platform = spawn_fake_telegram(update(7, 42, 200, None, "hello"), 1);
        let mut gateway = test_gateway(
            &workspace,
            socket_path,
            &platform.base_url,
            telegram_config(),
        );

        gateway.poll_once().unwrap();

        daemon.join().unwrap();
        let requests = platform.handle.join().unwrap();
        assert_eq!(requests[1].body["text"], "answer after lag");
    }

    #[test]
    fn telegram_errors_never_include_the_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _request = read_http_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let client = TelegramClient::new(&base_url, "secret-token".into(), 0);

        let error = client.get_updates(None).unwrap_err();
        handle.join().unwrap();

        assert!(!error.to_string().contains("secret-token"));
    }

    fn telegram_config() -> TelegramGatewayConfig {
        TelegramGatewayConfig {
            api_key_env: "TELEGRAM_BOT_TOKEN".into(),
            owner_user_ids: vec![42],
        }
    }

    fn test_gateway(
        workspace: &tempfile::TempDir,
        socket_path: PathBuf,
        base_url: &str,
        config: TelegramGatewayConfig,
    ) -> TelegramGateway {
        let daemon = DaemonConnectionConfig::resolve(workspace.path(), Some(socket_path)).unwrap();
        let mut gateway = TelegramGateway::new(
            TelegramClient::new(base_url, "test-token".into(), 0),
            daemon,
            None,
            config,
        );
        gateway.event_poll_delay = Duration::ZERO;
        gateway.reconnect_delay = Duration::from_millis(5);
        gateway
    }

    fn update(
        update_id: i64,
        user_id: i64,
        chat_id: i64,
        thread_id: Option<i64>,
        text: &str,
    ) -> Value {
        json!([{
            "update_id": update_id,
            "message": {
                "from": {"id": user_id},
                "chat": {"id": chat_id},
                "message_thread_id": thread_id,
                "text": text
            }
        }])
    }

    struct FakeTelegram {
        base_url: String,
        handle: thread::JoinHandle<Vec<HttpRequest>>,
    }

    struct HttpRequest {
        path: String,
        body: Value,
    }

    fn spawn_fake_telegram(updates: Value, expected_sends: usize) -> FakeTelegram {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let expected_requests = expected_sends + 1;
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut requests = Vec::new();
            while requests.len() < expected_requests && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("telegram accept failed: {error}"),
                };
                let request = read_http_request(&mut stream);
                let response = if request.path.contains("/getUpdates") {
                    json!({"ok": true, "result": updates})
                } else {
                    json!({"ok": true, "result": {}})
                };
                write_http_response(&mut stream, &response);
                requests.push(request);
            }
            assert_eq!(requests.len(), expected_requests);
            requests
        });
        FakeTelegram { base_url, handle }
    }

    fn read_http_request(stream: &mut TcpStream) -> HttpRequest {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let path = request_line.split_whitespace().nth(1).unwrap().to_owned();
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
        HttpRequest {
            path,
            body: if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            },
        }
    }

    fn write_http_response(stream: &mut TcpStream, body: &Value) {
        let body = serde_json::to_vec(body).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    }

    fn spawn_finished_daemon(socket_path: &Path, answer: &str) -> thread::JoinHandle<Value> {
        let listener = UnixListener::bind(socket_path).unwrap();
        let answer = answer.to_owned();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            respond_hello(&mut reader, &mut writer);
            let append = read_daemon_request(&mut reader);
            assert_eq!(append.method.as_deref(), Some("message.append"));
            write_daemon_response(
                &mut writer,
                append.id,
                "message.append",
                json!({
                    "run_id": "run_1",
                    "session_id": "session_1",
                    "ledger_path": "/tmp/agent.db",
                    "status": "running",
                    "final_answer": null
                }),
            );
            let events = read_daemon_request(&mut reader);
            assert_eq!(events.method.as_deref(), Some("events.stream"));
            write_daemon_response(
                &mut writer,
                events.id,
                "events.stream",
                json!({
                    "run_id": "run_1",
                    "from_offset": 0,
                    "next_offset": 1,
                    "status": "finished",
                    "events": []
                }),
            );
            let transcript = read_daemon_request(&mut reader);
            assert_eq!(transcript.method.as_deref(), Some("transcript.read"));
            write_daemon_response(
                &mut writer,
                transcript.id,
                "transcript.read",
                json!({
                    "run_id": "run_1",
                    "status": "finished",
                    "final_answer": answer,
                    "transcript": "rendered text must not be parsed"
                }),
            );
            append.params.unwrap()
        })
    }

    fn spawn_restarting_daemon(socket_path: &Path, answer: &str) -> thread::JoinHandle<()> {
        let first_listener = UnixListener::bind(socket_path).unwrap();
        let socket_path = socket_path.to_path_buf();
        let answer = answer.to_owned();
        thread::spawn(move || {
            {
                let (stream, _) = first_listener.accept().unwrap();
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                respond_hello(&mut reader, &mut writer);
                let append = read_daemon_request(&mut reader);
                write_daemon_response(
                    &mut writer,
                    append.id,
                    "message.append",
                    json!({
                        "run_id": "run_1",
                        "session_id": "session_1",
                        "ledger_path": "/tmp/agent.db",
                        "status": "running",
                        "final_answer": null
                    }),
                );
                let events = read_daemon_request(&mut reader);
                assert_eq!(events.method.as_deref(), Some("events.stream"));
            }
            drop(first_listener);
            std::fs::remove_file(&socket_path).unwrap();
            let second_listener = UnixListener::bind(&socket_path).unwrap();
            let (stream, _) = second_listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            respond_hello(&mut reader, &mut writer);
            let sessions = read_daemon_request(&mut reader);
            assert_eq!(sessions.method.as_deref(), Some("sessions.list"));
            write_daemon_response(
                &mut writer,
                sessions.id,
                "sessions.list",
                json!({
                    "sessions": [{
                        "session_id": "session_1",
                        "run_id": "run_1",
                        "status": "finished",
                        "latest_question": "hello",
                        "ledger_path": "/tmp/agent.db"
                    }]
                }),
            );
            let transcript = read_daemon_request(&mut reader);
            assert_eq!(transcript.method.as_deref(), Some("transcript.read"));
            write_daemon_response(
                &mut writer,
                transcript.id,
                "transcript.read",
                json!({
                    "run_id": "run_1",
                    "status": "finished",
                    "final_answer": answer,
                    "transcript": "not the answer"
                }),
            );
        })
    }

    fn spawn_lagged_daemon(socket_path: &Path, answer: &str) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket_path).unwrap();
        let answer = answer.to_owned();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            respond_hello(&mut reader, &mut writer);
            let append = read_daemon_request(&mut reader);
            write_daemon_response(
                &mut writer,
                append.id,
                "message.append",
                json!({
                    "run_id": "run_1",
                    "session_id": "session_1",
                    "ledger_path": "/tmp/agent.db",
                    "status": "running",
                    "final_answer": null
                }),
            );
            let lagged = read_daemon_request(&mut reader);
            assert_eq!(lagged.params.as_ref().unwrap()["from_offset"], 0);
            write_daemon_error(&mut writer, lagged.id, "events.stream", "lagged");
            let resumed = read_daemon_request(&mut reader);
            assert!(
                resumed
                    .params
                    .as_ref()
                    .unwrap()
                    .get("from_offset")
                    .is_none()
            );
            write_daemon_response(
                &mut writer,
                resumed.id,
                "events.stream",
                json!({
                    "run_id": "run_1",
                    "from_offset": 3,
                    "next_offset": 3,
                    "status": "finished",
                    "events": []
                }),
            );
            let transcript = read_daemon_request(&mut reader);
            write_daemon_response(
                &mut writer,
                transcript.id,
                "transcript.read",
                json!({
                    "run_id": "run_1",
                    "status": "finished",
                    "final_answer": answer,
                    "transcript": "not the answer"
                }),
            );
        })
    }

    fn respond_hello(reader: &mut BufReader<UnixStream>, writer: &mut UnixStream) {
        let hello = read_daemon_request(reader);
        assert_eq!(hello.method.as_deref(), Some("hello"));
        write_daemon_response(
            writer,
            hello.id,
            "hello",
            json!({
                "daemon_version": "test",
                "workspace_id": "workspace_1",
                "ledger_path": "/tmp/agent.db",
                "capabilities": REQUIRED_CAPABILITIES
            }),
        );
    }

    fn read_daemon_request(reader: &mut BufReader<UnixStream>) -> Envelope {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    fn write_daemon_response(
        writer: &mut UnixStream,
        id: Option<String>,
        method: &str,
        result: Value,
    ) {
        serde_json::to_writer(
            &mut *writer,
            &Envelope {
                v: PROTOCOL_VERSION,
                id,
                kind: EnvelopeKind::Response,
                method: Some(method.into()),
                params: None,
                result: Some(result),
                error: None,
            },
        )
        .unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
    }

    fn write_daemon_error(writer: &mut UnixStream, id: Option<String>, method: &str, code: &str) {
        serde_json::to_writer(
            &mut *writer,
            &Envelope::error(id, Some(method.into()), code, "test error"),
        )
        .unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
    }
}
