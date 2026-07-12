use crate::{
    AppError, AppResult,
    config::{Config, DiscordGatewayConfig, resolve_config_path},
    daemon::{
        client::{DaemonClient, DaemonConnectionConfig},
        protocol::{HelloResult, RunStateName, TranscriptReadResult},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    net::TcpStream,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{Message, WebSocket, connect, stream::MaybeTlsStream};
use url::Url;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);
const DISCORD_MESSAGE_LIMIT: usize = 2_000;
const GATEWAY_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const GATEWAY_READ_TIMEOUT: Duration = Duration::from_millis(100);
const GATEWAY_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const EVENT_PAGE_LIMIT: usize = 64;
const EVENT_POLL_DELAY: Duration = Duration::from_millis(100);
const RECONNECT_ATTEMPTS: usize = 40;
const RECONNECT_DELAY: Duration = Duration::from_millis(50);
const REQUIRED_CAPABILITIES: [&str; 6] = [
    "hello",
    "run.start",
    "message.append",
    "events.stream",
    "sessions.list",
    "transcript.read",
];

pub struct DiscordGatewayOptions {
    pub workspace_root: PathBuf,
    pub socket_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
}

pub fn run_discord_gateway(options: DiscordGatewayOptions) -> AppResult<()> {
    let config_path = resolve_config_path(&options.workspace_root, options.config_path.as_deref())?;
    let config = Config::load(&options.workspace_root, config_path.as_deref())?;
    let discord = config
        .gateway
        .clone()
        .map(|gateway| gateway.discord)
        .ok_or_else(|| AppError::Config("gateway.discord configuration is required".into()))?;
    let token = gateway_token(&config, &discord, |name| std::env::var_os(name))?;
    let daemon = DaemonConnectionConfig::resolve(&options.workspace_root, options.socket_path)?;
    let platform = DiscordPlatform::connect(DISCORD_API_BASE, token)?;
    let config_path = config_path.map(|path| path.to_string_lossy().into_owned());
    DiscordGateway::new(platform, daemon, config_path, discord).run()
}

fn gateway_token(
    config: &Config,
    discord: &DiscordGatewayConfig,
    env: impl Fn(&str) -> Option<OsString>,
) -> AppResult<String> {
    let provider_envs = [
        config.provider.api_key_env.as_str(),
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
    ];
    if provider_envs
        .iter()
        .any(|name| *name == discord.api_key_env)
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
    let token = env(&discord.api_key_env).ok_or_else(|| {
        AppError::Config(format!(
            "gateway token env var {} is not set",
            discord.api_key_env
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

struct DiscordGateway {
    platform: DiscordPlatform,
    daemon: DaemonConnectionConfig,
    config_path: Option<String>,
    owner_user_ids: HashSet<u64>,
    sessions: HashMap<u64, String>,
    event_poll_delay: Duration,
    reconnect_delay: Duration,
}

impl DiscordGateway {
    fn new(
        platform: DiscordPlatform,
        daemon: DaemonConnectionConfig,
        config_path: Option<String>,
        config: DiscordGatewayConfig,
    ) -> Self {
        Self {
            platform,
            daemon,
            config_path,
            owner_user_ids: config.owner_user_ids.into_iter().collect(),
            sessions: HashMap::new(),
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
        let message = self.platform.recv_message()?;
        if !self.owner_user_ids.contains(&message.author_id) || message.content.trim().is_empty() {
            return Ok(());
        }
        self.handle_message(message.channel_id, message.content)
    }

    fn handle_message(&mut self, channel_id: u64, text: String) -> AppResult<()> {
        let mut daemon = self.connect_daemon()?;
        let run = match self.sessions.get(&channel_id).cloned() {
            Some(session_id) => daemon.message_append_to_session(
                text,
                Some(session_id),
                self.config_path.clone(),
                false,
            ),
            None => daemon.run_start(text, self.config_path.clone(), false),
        }?;
        self.sessions.insert(channel_id, run.session_id.clone());
        let answer = self.wait_for_run(&mut daemon, &run.run_id, &run.session_id)?;
        self.platform.send_message(channel_id, &answer)
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
                    if events.status != RunStateName::Running {
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
                    if session.status != RunStateName::Running {
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

struct DiscordPlatform {
    rest: DiscordRestClient,
    messages: Receiver<AppResult<DiscordMessage>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl DiscordPlatform {
    fn connect(api_base: &str, token: String) -> AppResult<Self> {
        let rest = DiscordRestClient::new(api_base, token.clone());
        let gateway_url = rest.gateway_url()?;
        let (sender, messages) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let receiver = DiscordGatewayReceiver {
            token,
            initial_url: gateway_url,
            read_timeout: GATEWAY_READ_TIMEOUT,
            reconnect_delay: GATEWAY_RECONNECT_DELAY,
        };
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("discord-gateway".into())
            .spawn(move || receiver.run(sender, worker_stop))?;
        Ok(Self {
            rest,
            messages,
            stop,
            worker: Some(worker),
        })
    }

    fn recv_message(&self) -> AppResult<DiscordMessage> {
        self.messages
            .recv()
            .map_err(|_| AppError::Provider("discord gateway receiver stopped".into()))?
    }

    fn send_message(&self, channel_id: u64, text: &str) -> AppResult<()> {
        self.rest.send_message(channel_id, text)
    }
}

impl Drop for DiscordPlatform {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct DiscordRestClient {
    agent: ureq::Agent,
    api_base: String,
    token: String,
}

impl DiscordRestClient {
    fn new(api_base: &str, token: String) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(35))
                .build(),
            api_base: api_base.trim_end_matches('/').into(),
            token,
        }
    }

    fn gateway_url(&self) -> AppResult<String> {
        let response = self
            .request(self.agent.get(&format!("{}/gateway/bot", self.api_base)))
            .call()
            .map_err(|error| discord_http_error("gateway discovery", error))?;
        let response: GatewayBotResponse = response.into_json().map_err(|_| {
            AppError::Provider("discord gateway discovery returned invalid JSON".into())
        })?;
        if response.url.is_empty() {
            return Err(AppError::Provider(
                "discord gateway discovery returned an empty URL".into(),
            ));
        }
        Ok(response.url)
    }

    fn send_message(&self, channel_id: u64, text: &str) -> AppResult<()> {
        for content in discord_chunks(text) {
            self.request(
                self.agent
                    .post(&format!("{}/channels/{channel_id}/messages", self.api_base)),
            )
            .send_json(CreateMessage {
                content,
                allowed_mentions: AllowedMentions { parse: Vec::new() },
            })
            .map_err(|error| discord_http_error("message send", error))?;
        }
        Ok(())
    }

    fn request(&self, request: ureq::Request) -> ureq::Request {
        request
            .set("Authorization", &format!("Bot {}", self.token))
            .set("User-Agent", "plato-agent/0.1")
    }
}

fn discord_http_error(operation: &str, error: ureq::Error) -> AppError {
    match error {
        ureq::Error::Status(status, _) => {
            AppError::Provider(format!("discord {operation} returned HTTP {status}"))
        }
        ureq::Error::Transport(_) => {
            AppError::Provider(format!("discord {operation} transport failed"))
        }
    }
}

fn discord_chunks(text: &str) -> Vec<String> {
    let characters = text.chars().collect::<Vec<_>>();
    characters
        .chunks(DISCORD_MESSAGE_LIMIT)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[derive(Deserialize)]
struct GatewayBotResponse {
    url: String,
}

#[derive(Serialize)]
struct CreateMessage {
    content: String,
    allowed_mentions: AllowedMentions,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiscordMessage {
    channel_id: u64,
    author_id: u64,
    content: String,
}

struct DiscordGatewayReceiver {
    token: String,
    initial_url: String,
    read_timeout: Duration,
    reconnect_delay: Duration,
}

impl DiscordGatewayReceiver {
    fn run(self, sender: Sender<AppResult<DiscordMessage>>, stop: Arc<AtomicBool>) {
        let mut session = None;
        while !stop.load(Ordering::Relaxed) {
            match self.run_connection(&sender, &stop, &mut session) {
                GatewayControl::Resume => {}
                GatewayControl::Reidentify => session = None,
                GatewayControl::Fatal(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
                GatewayControl::Stop => return,
            }
            if !stop.load(Ordering::Relaxed) {
                thread::sleep(self.reconnect_delay);
            }
        }
    }

    fn run_connection(
        &self,
        sender: &Sender<AppResult<DiscordMessage>>,
        stop: &AtomicBool,
        session: &mut Option<DiscordSession>,
    ) -> GatewayControl {
        let base_url = session
            .as_ref()
            .map(|session| session.resume_gateway_url.as_str())
            .unwrap_or(&self.initial_url);
        let url = match gateway_url(base_url) {
            Ok(url) => url,
            Err(error) => return GatewayControl::Fatal(error),
        };
        let (mut socket, _) = match connect(url.as_str()) {
            Ok(connection) => connection,
            Err(tungstenite::Error::Url(_)) => {
                return GatewayControl::Fatal(AppError::Provider(
                    "discord gateway returned an invalid websocket URL".into(),
                ));
            }
            Err(_) => return GatewayControl::Resume,
        };
        if set_read_timeout(&mut socket, self.read_timeout).is_err() {
            return GatewayControl::Resume;
        }
        let heartbeat_interval = match wait_for_hello(&mut socket, stop) {
            Ok(interval) => interval,
            Err(control) => return control,
        };
        if let Some(current) = session.as_ref() {
            if send_gateway_payload(
                &mut socket,
                &json!({
                    "op": 6,
                    "d": {
                        "token": self.token,
                        "session_id": current.session_id,
                        "seq": current.sequence
                    }
                }),
            )
            .is_err()
            {
                return GatewayControl::Resume;
            }
        } else if send_gateway_payload(
            &mut socket,
            &json!({
                "op": 2,
                "d": {
                    "token": self.token,
                    "intents": DISCORD_INTENTS,
                    "properties": {
                        "os": std::env::consts::OS,
                        "browser": "plato-agent",
                        "device": "plato-agent"
                    }
                }
            }),
        )
        .is_err()
        {
            return GatewayControl::Resume;
        }

        let mut sequence = session.as_ref().map(|session| session.sequence);
        let mut heartbeat_acknowledged = true;
        let mut next_heartbeat = Instant::now() + heartbeat_jitter(heartbeat_interval);
        loop {
            if stop.load(Ordering::Relaxed) {
                return GatewayControl::Stop;
            }
            if Instant::now() >= next_heartbeat {
                if !heartbeat_acknowledged {
                    return GatewayControl::Resume;
                }
                if send_gateway_payload(&mut socket, &json!({"op": 1, "d": sequence})).is_err() {
                    return GatewayControl::Resume;
                }
                heartbeat_acknowledged = false;
                next_heartbeat = Instant::now() + heartbeat_interval;
            }
            let payload = match read_gateway_payload(&mut socket) {
                Ok(Some(payload)) => payload,
                Ok(None) => continue,
                Err(control) => return control,
            };
            if let Some(value) = payload.s {
                sequence = Some(value);
                if let Some(current) = session.as_mut() {
                    current.sequence = value;
                }
            }
            match payload.op {
                0 => match payload.t.as_deref() {
                    Some("READY") => {
                        let ready: ReadyEvent = match serde_json::from_value(payload.d) {
                            Ok(ready) => ready,
                            Err(_) => return invalid_gateway_payload(),
                        };
                        let Some(sequence) = sequence else {
                            return invalid_gateway_payload();
                        };
                        *session = Some(DiscordSession {
                            session_id: ready.session_id,
                            resume_gateway_url: ready.resume_gateway_url,
                            sequence,
                        });
                    }
                    Some("MESSAGE_CREATE") => {
                        let message: MessageCreateEvent = match serde_json::from_value(payload.d) {
                            Ok(message) => message,
                            Err(_) => return invalid_gateway_payload(),
                        };
                        if message.author.bot.unwrap_or(false) {
                            continue;
                        }
                        let channel_id = match parse_snowflake(&message.channel_id) {
                            Ok(value) => value,
                            Err(error) => return GatewayControl::Fatal(error),
                        };
                        let author_id = match parse_snowflake(&message.author.id) {
                            Ok(value) => value,
                            Err(error) => return GatewayControl::Fatal(error),
                        };
                        if sender
                            .send(Ok(DiscordMessage {
                                channel_id,
                                author_id,
                                content: message.content,
                            }))
                            .is_err()
                        {
                            return GatewayControl::Stop;
                        }
                    }
                    _ => {}
                },
                1 => {
                    if send_gateway_payload(&mut socket, &json!({"op": 1, "d": sequence})).is_err()
                    {
                        return GatewayControl::Resume;
                    }
                    heartbeat_acknowledged = false;
                    next_heartbeat = Instant::now() + heartbeat_interval;
                }
                7 => return GatewayControl::Resume,
                9 => {
                    return if payload.d.as_bool().unwrap_or(false) {
                        GatewayControl::Resume
                    } else {
                        GatewayControl::Reidentify
                    };
                }
                10 => {}
                11 => heartbeat_acknowledged = true,
                _ => {}
            }
        }
    }
}

enum GatewayControl {
    Resume,
    Reidentify,
    Fatal(AppError),
    Stop,
}

struct DiscordSession {
    session_id: String,
    resume_gateway_url: String,
    sequence: u64,
}

#[derive(Deserialize)]
struct GatewayPayload {
    op: u8,
    #[serde(default)]
    d: Value,
    s: Option<u64>,
    t: Option<String>,
}

#[derive(Deserialize)]
struct ReadyEvent {
    session_id: String,
    resume_gateway_url: String,
}

#[derive(Deserialize)]
struct MessageCreateEvent {
    channel_id: String,
    author: DiscordAuthor,
    content: String,
}

#[derive(Deserialize)]
struct DiscordAuthor {
    id: String,
    bot: Option<bool>,
}

fn gateway_url(base_url: &str) -> AppResult<String> {
    let mut url = Url::parse(base_url).map_err(|_| {
        AppError::Provider("discord gateway returned an invalid websocket URL".into())
    })?;
    url.query_pairs_mut()
        .clear()
        .append_pair("v", "10")
        .append_pair("encoding", "json");
    Ok(url.to_string())
}

fn wait_for_hello(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    stop: &AtomicBool,
) -> Result<Duration, GatewayControl> {
    let deadline = Instant::now() + GATEWAY_HELLO_TIMEOUT;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        let Some(payload) = read_gateway_payload(socket)? else {
            continue;
        };
        if payload.op != 10 {
            return Err(invalid_gateway_payload());
        }
        let interval = payload
            .d
            .get("heartbeat_interval")
            .and_then(Value::as_u64)
            .filter(|interval| *interval > 0)
            .ok_or_else(invalid_gateway_payload)?;
        return Ok(Duration::from_millis(interval));
    }
    if stop.load(Ordering::Relaxed) {
        Err(GatewayControl::Stop)
    } else {
        Err(GatewayControl::Resume)
    }
}

fn read_gateway_payload(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> Result<Option<GatewayPayload>, GatewayControl> {
    match socket.read() {
        Ok(Message::Text(text)) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| invalid_gateway_payload()),
        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
            let _ = socket.flush();
            Ok(None)
        }
        Ok(Message::Close(frame)) => Err(close_control(frame.map(|frame| frame.code.into()))),
        Ok(_) => Err(invalid_gateway_payload()),
        Err(tungstenite::Error::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
            Err(GatewayControl::Resume)
        }
        Err(_) => Err(GatewayControl::Resume),
    }
}

fn send_gateway_payload(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    payload: &Value,
) -> Result<(), ()> {
    let payload = serde_json::to_string(payload).map_err(|_| ())?;
    socket.send(Message::Text(payload.into())).map_err(|_| ())
}

fn close_control(code: Option<u16>) -> GatewayControl {
    match code {
        Some(4004 | 4010 | 4011 | 4012 | 4013 | 4014) => GatewayControl::Fatal(AppError::Provider(
            format!("discord gateway closed with fatal code {}", code.unwrap()),
        )),
        Some(4007 | 4009) => GatewayControl::Reidentify,
        _ => GatewayControl::Resume,
    }
}

fn invalid_gateway_payload() -> GatewayControl {
    GatewayControl::Fatal(AppError::Provider(
        "discord gateway returned an invalid payload".into(),
    ))
}

fn parse_snowflake(value: &str) -> AppResult<u64> {
    value
        .parse()
        .map_err(|_| AppError::Provider("discord gateway returned an invalid snowflake".into()))
}

fn heartbeat_jitter(interval: Duration) -> Duration {
    let upper = interval.as_millis() as u64;
    if upper == 0 {
        return Duration::ZERO;
    }
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    Duration::from_millis(seed % upper)
}

fn set_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> std::io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(Some(timeout)),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(Some(timeout)),
        _ => Err(std::io::Error::other(
            "unsupported discord websocket transport",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{Envelope, EnvelopeKind, PROTOCOL_VERSION};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        os::unix::net::{UnixListener, UnixStream},
        path::Path,
        time::Instant,
    };
    use tungstenite::accept;

    #[test]
    fn gateway_environment_rejects_provider_credentials() {
        let config = Config::default();
        let discord = discord_config();

        let error = gateway_token(&config, &discord, |name| match name {
            "DISCORD_BOT_TOKEN" => Some(OsString::from("discord-secret")),
            "OPENROUTER_API_KEY" => Some(OsString::from("provider-secret")),
            _ => None,
        })
        .unwrap_err();

        assert!(error.to_string().contains("OPENROUTER_API_KEY"));
        assert!(!error.to_string().contains("provider-secret"));
        assert!(!error.to_string().contains("discord-secret"));
    }

    #[test]
    fn non_owner_messages_are_silently_ignored() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let rest = spawn_fake_rest(0, 200, None);
        let platform = test_platform(&rest.base_url, discord_message(99, 200, "ignore me"));
        let mut gateway =
            test_gateway(&workspace, socket_dir.path().join("missing.sock"), platform);

        gateway.poll_once().unwrap();

        assert!(rest.handle.join().unwrap().is_empty());
        assert!(gateway.sessions.is_empty());
    }

    #[test]
    fn owner_message_replies_with_typed_final_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("daemon.sock");
        let daemon = spawn_finished_daemon(&socket_path, "run.start", "session_1", "final answer");
        let rest = spawn_fake_rest(1, 200, None);
        let platform = test_platform(&rest.base_url, discord_message(42, 200, "hello"));
        let mut gateway = test_gateway(&workspace, socket_path, platform);

        gateway.poll_once().unwrap();

        let start_params = daemon.join().unwrap();
        assert_eq!(start_params["question"], "hello");
        assert!(start_params.get("session_id").is_none());
        assert_eq!(start_params["wait"], false);
        let requests = rest.handle.join().unwrap();
        assert_eq!(requests[0].path, "/channels/200/messages");
        assert_eq!(requests[0].authorization, "Bot test-token");
        assert_eq!(requests[0].body["content"], "final answer");
        assert_eq!(requests[0].body["allowed_mentions"]["parse"], json!([]));
        assert_eq!(gateway.sessions[&200], "session_1");
    }

    #[test]
    fn owner_followup_appends_to_channel_session() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("daemon.sock");
        let daemon = spawn_finished_daemon(
            &socket_path,
            "message.append",
            "session_existing",
            "next answer",
        );
        let rest = spawn_fake_rest(1, 200, None);
        let platform = test_platform(&rest.base_url, discord_message(42, 200, "follow up"));
        let mut gateway = test_gateway(&workspace, socket_path, platform);
        gateway.sessions.insert(200, "session_existing".into());

        gateway.poll_once().unwrap();

        let append_params = daemon.join().unwrap();
        assert_eq!(append_params["message"], "follow up");
        assert_eq!(append_params["session_id"], "session_existing");
        assert_eq!(append_params["wait"], false);
        let requests = rest.handle.join().unwrap();
        assert_eq!(requests[0].body["content"], "next answer");
        assert_eq!(gateway.sessions[&200], "session_existing");
    }

    #[test]
    fn daemon_restart_recovers_final_answer_from_typed_transcript() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("daemon.sock");
        let daemon = spawn_restarting_daemon(&socket_path, "recovered answer");
        let rest = spawn_fake_rest(1, 200, None);
        let platform = test_platform(&rest.base_url, discord_message(42, 200, "hello"));
        let mut gateway = test_gateway(&workspace, socket_path, platform);

        gateway.poll_once().unwrap();

        daemon.join().unwrap();
        let requests = rest.handle.join().unwrap();
        assert_eq!(requests[0].body["content"], "recovered answer");
    }

    #[test]
    fn lag_resumes_at_tip_and_reads_typed_final_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("daemon.sock");
        let daemon = spawn_lagged_daemon(&socket_path, "answer after lag");
        let rest = spawn_fake_rest(1, 200, None);
        let platform = test_platform(&rest.base_url, discord_message(42, 200, "hello"));
        let mut gateway = test_gateway(&workspace, socket_path, platform);

        gateway.poll_once().unwrap();

        daemon.join().unwrap();
        let requests = rest.handle.join().unwrap();
        assert_eq!(requests[0].body["content"], "answer after lag");
    }

    #[test]
    fn discord_http_errors_never_include_the_token() {
        let rest = spawn_fake_rest(1, 401, None);
        let client = DiscordRestClient::new(&rest.base_url, "secret-token".into());

        let error = client.send_message(200, "hello").unwrap_err();
        rest.handle.join().unwrap();

        assert!(!error.to_string().contains("secret-token"));
    }

    #[test]
    fn websocket_identifies_and_receives_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let websocket_url = format!("ws://{}", listener.local_addr().unwrap());
        let rest = spawn_fake_rest(1, 200, Some(websocket_url.clone()));
        let websocket = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut socket = accept(stream).unwrap();
            send_websocket_json(
                &mut socket,
                json!({"op": 10, "d": {"heartbeat_interval": 20}}),
            );
            let identify = read_websocket_json(&mut socket);
            assert_eq!(identify["op"], 2);
            assert_eq!(identify["d"]["token"], "test-token");
            assert_eq!(identify["d"]["intents"], DISCORD_INTENTS);
            send_websocket_json(
                &mut socket,
                json!({
                    "op": 0,
                    "s": 1,
                    "t": "READY",
                    "d": {
                        "session_id": "discord_session",
                        "resume_gateway_url": websocket_url
                    }
                }),
            );
            send_websocket_json(
                &mut socket,
                json!({
                    "op": 0,
                    "s": 2,
                    "t": "MESSAGE_CREATE",
                    "d": {
                        "channel_id": "200",
                        "author": {"id": "42", "bot": false},
                        "content": "hello"
                    }
                }),
            );
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                let payload = read_websocket_json(&mut socket);
                if payload["op"] == 1 {
                    send_websocket_json(&mut socket, json!({"op": 11, "d": null}));
                    if payload["d"] == 2 {
                        return;
                    }
                }
            }
            panic!("discord gateway did not send a heartbeat");
        });

        let platform = DiscordPlatform::connect(&rest.base_url, "test-token".into()).unwrap();
        let message = platform.recv_message().unwrap();

        assert_eq!(message, discord_message(42, 200, "hello"));
        websocket.join().unwrap();
        drop(platform);
        let requests = rest.handle.join().unwrap();
        assert_eq!(requests[0].path, "/gateway/bot");
        assert_eq!(requests[0].authorization, "Bot test-token");
    }

    fn discord_config() -> DiscordGatewayConfig {
        DiscordGatewayConfig {
            api_key_env: "DISCORD_BOT_TOKEN".into(),
            owner_user_ids: vec![42],
        }
    }

    fn discord_message(author_id: u64, channel_id: u64, content: &str) -> DiscordMessage {
        DiscordMessage {
            channel_id,
            author_id,
            content: content.into(),
        }
    }

    fn test_platform(api_base: &str, message: DiscordMessage) -> DiscordPlatform {
        let (sender, messages) = mpsc::channel();
        sender.send(Ok(message)).unwrap();
        DiscordPlatform {
            rest: DiscordRestClient::new(api_base, "test-token".into()),
            messages,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    fn test_gateway(
        workspace: &tempfile::TempDir,
        socket_path: PathBuf,
        platform: DiscordPlatform,
    ) -> DiscordGateway {
        let daemon = DaemonConnectionConfig::resolve(workspace.path(), Some(socket_path)).unwrap();
        let mut gateway = DiscordGateway::new(platform, daemon, None, discord_config());
        gateway.event_poll_delay = Duration::ZERO;
        gateway.reconnect_delay = Duration::from_millis(5);
        gateway
    }

    struct FakeRest {
        base_url: String,
        handle: thread::JoinHandle<Vec<HttpRequest>>,
    }

    struct HttpRequest {
        path: String,
        authorization: String,
        body: Value,
    }

    fn spawn_fake_rest(
        expected_requests: usize,
        status: u16,
        gateway_url: Option<String>,
    ) -> FakeRest {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut requests = Vec::new();
            while requests.len() < expected_requests && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("discord REST accept failed: {error}"),
                };
                let request = read_http_request(&mut stream);
                let body = if request.path == "/gateway/bot" {
                    json!({"url": gateway_url})
                } else {
                    json!({"id": "message_1"})
                };
                write_http_response(&mut stream, status, &body);
                requests.push(request);
            }
            assert_eq!(requests.len(), expected_requests);
            requests
        });
        FakeRest { base_url, handle }
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
        let mut authorization = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap();
            }
            if line.to_ascii_lowercase().starts_with("authorization:") {
                authorization = line.split_once(':').unwrap().1.trim().to_owned();
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        HttpRequest {
            path,
            authorization,
            body: if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            },
        }
    }

    fn write_http_response(stream: &mut TcpStream, status: u16, body: &Value) {
        let body = serde_json::to_vec(body).unwrap();
        let reason = if status == 200 { "OK" } else { "Unauthorized" };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    }

    fn read_websocket_json(socket: &mut WebSocket<TcpStream>) -> Value {
        loop {
            match socket.read().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(payload) => socket.send(Message::Pong(payload)).unwrap(),
                _ => {}
            }
        }
    }

    fn send_websocket_json(socket: &mut WebSocket<TcpStream>, payload: Value) {
        socket
            .send(Message::Text(payload.to_string().into()))
            .unwrap();
    }

    fn spawn_finished_daemon(
        socket_path: &Path,
        method: &str,
        session_id: &str,
        answer: &str,
    ) -> thread::JoinHandle<Value> {
        let listener = UnixListener::bind(socket_path).unwrap();
        let method = method.to_owned();
        let session_id = session_id.to_owned();
        let answer = answer.to_owned();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            respond_hello(&mut reader, &mut writer);
            let request = read_daemon_request(&mut reader);
            assert_eq!(request.method.as_deref(), Some(method.as_str()));
            write_daemon_response(
                &mut writer,
                request.id,
                &method,
                json!({
                    "run_id": "run_1",
                    "session_id": session_id,
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
            request.params.unwrap()
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
                let start = read_daemon_request(&mut reader);
                assert_eq!(start.method.as_deref(), Some("run.start"));
                write_daemon_response(
                    &mut writer,
                    start.id,
                    "run.start",
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
            let start = read_daemon_request(&mut reader);
            assert_eq!(start.method.as_deref(), Some("run.start"));
            write_daemon_response(
                &mut writer,
                start.id,
                "run.start",
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
