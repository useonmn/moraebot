//! Slack adapter (Socket Mode).
//!
//! The protocol core (frame parsing, event mapping, id encoding, text escaping and Web
//! API response parsing) is pure. All I/O goes through the `SlackTransport` seam: the
//! real `HttpTransport` (HTTPS Web API + outbound WebSocket, no listening socket) and
//! `testing::FakeSlackTransport` for offline tests.
//!
//! Id encoding: a Slack `ts` is unique only per channel, so message ids are
//! `<channel>:<ts>` and thread ids are `<channel>:<thread_ts>`.
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::adapter::{
    AdapterError, E_ADAPTER_SEND, E_ADAPTER_START, InboundEvent, MessengerAdapter, SendResult,
    TypingHandle, next_backoff,
};
use crate::config::{Config, E_MESSENGER_MISSING};

pub const PLATFORM: &str = "slack";
/// Slack advises keeping `text` under 4,000 characters.
pub const MESSAGE_LIMIT: usize = 3900;

pub const E_SLACK_AUTH: &str = "E_SLACK_AUTH";
pub const E_SLACK_APP_TOKEN: &str = "E_SLACK_APP_TOKEN";
pub const E_SLACK_API: &str = "E_SLACK_API";
pub const E_SLACK_RATE: &str = "E_SLACK_RATE";
pub const E_SLACK_ID: &str = "E_SLACK_ID";
pub const E_SLACK_PROTOCOL: &str = "E_SLACK_PROTOCOL";

pub const DEFAULT_HINTS: &str = "Your replies are posted to Slack. Use Slack mrkdwn, not Markdown: *bold*, _italic_, ~strike~, `code`, fenced code blocks, > quotes, - lists. Double-asterisk bold, # headings and tables do not render. Write links as plain URLs. One message holds about 3900 characters; longer replies are split.";

/// One Socket Mode message received from Slack.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello,
    Disconnect { reason: String },
    Event { envelope_id: String, payload: Value },
    Other { envelope_id: Option<String> },
}

/// Who may reach the bot and where it listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub owner_id: String,
    pub channel_id: String,
    pub bot_user_id: String,
}

fn protocol_error(message: impl Into<String>) -> AdapterError {
    AdapterError {
        code: E_SLACK_PROTOCOL,
        message: message.into(),
    }
}

pub fn parse_frame(text: &str) -> Result<Frame, AdapterError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| protocol_error(format!("bad frame: {e}")))?;
    let envelope_id = value
        .get("envelope_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    match value.get("type").and_then(Value::as_str) {
        Some("hello") => Ok(Frame::Hello),
        Some("disconnect") => Ok(Frame::Disconnect {
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        Some("events_api") => {
            let envelope_id =
                envelope_id.ok_or_else(|| protocol_error("events_api without envelope_id"))?;
            let payload = value
                .get("payload")
                .cloned()
                .ok_or_else(|| protocol_error("events_api without payload"))?;
            Ok(Frame::Event {
                envelope_id,
                payload,
            })
        }
        _ => Ok(Frame::Other { envelope_id }),
    }
}

pub fn ack_frame(envelope_id: &str) -> String {
    json!({ "envelope_id": envelope_id }).to_string()
}

/// Maps an `events_api` payload to an inbound event, or `None` when the message must be
/// dropped: wrong type, bot or own messages, edits and other subtypes, other users in a
/// DM, other channels. Every field is optional on the wire; a missing one drops the event.
pub fn map_event(payload: &Value, scope: &Scope) -> Option<InboundEvent> {
    if payload.get("type")?.as_str()? != "event_callback" {
        return None;
    }
    let event = payload.get("event")?;
    if event.get("type")?.as_str()? != "message" {
        return None;
    }
    if let Some(subtype) = event.get("subtype") {
        if subtype.as_str() != Some("thread_broadcast") {
            return None;
        }
    }
    if event.get("bot_id").is_some() {
        return None;
    }
    let user = event.get("user")?.as_str()?;
    if user == scope.bot_user_id {
        return None;
    }
    let channel = event.get("channel")?.as_str()?;
    let ts = event.get("ts")?.as_str()?;
    let is_dm = event.get("channel_type").and_then(Value::as_str) == Some("im");
    let thread_id = if is_dm {
        if user != scope.owner_id {
            return None;
        }
        None
    } else {
        if channel != scope.channel_id {
            return None;
        }
        event
            .get("thread_ts")
            .and_then(Value::as_str)
            .filter(|thread_ts| *thread_ts != ts)
            .map(|thread_ts| format!("{channel}:{thread_ts}"))
    };
    let raw_text = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mention = format!("<@{}>", scope.bot_user_id);
    let text = match raw_text.strip_prefix(&mention) {
        Some(rest) => rest.trim_start(),
        None => raw_text,
    };
    Some(InboundEvent {
        message_id: format!("{channel}:{ts}"),
        author_id: user.to_string(),
        author_is_bot: false,
        channel_id: channel.to_string(),
        thread_id,
        is_dm,
        text: text.to_string(),
        attachments: Vec::new(),
    })
}

/// Splits a send target: `<channel>:<ts>` posts into that thread, a plain channel or DM
/// id posts to the channel itself.
pub fn parse_target(target: &str) -> Result<(String, Option<String>), AdapterError> {
    let bad = || AdapterError {
        code: E_SLACK_ID,
        message: "send target must be <channel> or <channel>:<ts>".to_string(),
    };
    match target.split_once(':') {
        None if target.is_empty() => Err(bad()),
        None => Ok((target.to_string(), None)),
        Some((channel, ts)) if !channel.is_empty() && !ts.is_empty() => {
            Ok((channel.to_string(), Some(ts.to_string())))
        }
        Some(_) => Err(bad()),
    }
}

/// Slack text escapes only these three characters.
pub fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn post_body(channel: &str, thread_ts: Option<&str>, text: &str) -> Value {
    let mut body = json!({ "channel": channel, "text": text });
    if let Some(thread_ts) = thread_ts {
        body["thread_ts"] = json!(thread_ts);
    }
    body
}

/// Checks `ok` and returns the field `key`. On `ok: false` the error carries Slack's
/// `error` string with `code`; a missing field on an ok response is a protocol error.
fn ok_field(json: &Value, key: &str, code: &'static str) -> Result<String, AdapterError> {
    if json.get("ok").and_then(Value::as_bool) != Some(true) {
        let reason = json
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        return Err(AdapterError {
            code,
            message: reason.to_string(),
        });
    }
    json.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| protocol_error(format!("response without {key}")))
}

/// `apps.connections.open` response -> the `wss://` URL.
pub fn parse_open_response(json: &Value) -> Result<String, AdapterError> {
    ok_field(json, "url", E_SLACK_APP_TOKEN)
}

/// `auth.test` response -> the bot's user id.
pub fn parse_auth_response(json: &Value) -> Result<String, AdapterError> {
    ok_field(json, "user_id", E_SLACK_AUTH)
}

/// `chat.postMessage` response -> the new message's `ts`.
pub fn parse_post_response(json: &Value) -> Result<String, AdapterError> {
    ok_field(json, "ts", E_SLACK_API)
}

const AUTH_TEST_URL: &str = "https://slack.com/api/auth.test";
const CONNECTIONS_OPEN_URL: &str = "https://slack.com/api/apps.connections.open";
const POST_MESSAGE_URL: &str = "https://slack.com/api/chat.postMessage";

const INBOUND_BUFFER: usize = 64;
const BACKOFF_START: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// A connection that lasted at least this long resets the reconnect backoff.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);
const PING_EVERY: Duration = Duration::from_secs(30);
/// No message at all (pong included) for this long means the link is dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
/// `chat.postMessage` attempts per send while Slack answers HTTP 429.
const SEND_ATTEMPTS: usize = 3;
const RETRY_AFTER_DEFAULT: Duration = Duration::from_secs(1);
const RETRY_AFTER_MAX: Duration = Duration::from_secs(60);
/// Socket Mode turned off in the app settings; reconnecting at once would only hit
/// `apps.connections.open` (Tier 3) in a loop.
const DISCONNECT_LINK_DISABLED: &str = "link_disabled";

/// Result of one `chat.postMessage` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// Posted; carries the new message's `ts`.
    Posted(String),
    /// HTTP 429; carries how long to wait before retrying.
    RateLimited(Duration),
}

/// One message read from the Socket Mode WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketMessage {
    Text(String),
    /// A pong to our ping, or a ping from Slack: either proves the link is alive.
    Pong,
    Close,
}

/// Everything the adapter does on the network. Tokens stay inside the implementation.
#[async_trait]
pub trait SlackTransport: Send + Sync {
    /// `auth.test` with the bot token -> the bot's user id.
    async fn auth_test(&self) -> Result<String, AdapterError>;
    /// `apps.connections.open` with the app token -> a fresh `wss://` URL.
    async fn open_connection(&self) -> Result<String, AdapterError>;
    async fn connect(&self, url: &str) -> Result<Box<dyn SlackSocket>, AdapterError>;
    /// `chat.postMessage` with the bot token.
    async fn post_message(&self, body: Value) -> Result<PostOutcome, AdapterError>;
}

/// An open Socket Mode connection.
#[async_trait]
pub trait SlackSocket: Send {
    /// The next message; `None` when the connection ended.
    async fn next(&mut self) -> Option<Result<SocketMessage, AdapterError>>;
    async fn send_text(&mut self, text: String) -> Result<(), AdapterError>;
    async fn ping(&mut self) -> Result<(), AdapterError>;
    /// Closes the connection; errors are ignored because the socket is dropped anyway.
    async fn close(&mut self);
}

/// A token read from a 0600 file. Its `Debug` never shows the value.
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

fn read_secret(path: &Path, code: &'static str) -> Result<Secret, AdapterError> {
    let text = std::fs::read_to_string(path).map_err(|err| AdapterError {
        code,
        message: format!(
            "cannot read the token file {}: {}",
            path.display(),
            err.kind()
        ),
    })?;
    let token = text.trim();
    if token.is_empty() {
        return Err(AdapterError {
            code,
            message: format!("the token file {} is empty", path.display()),
        });
    }
    Ok(Secret(token.to_owned()))
}

fn api_error(code: &'static str, err: impl fmt::Display) -> AdapterError {
    AdapterError {
        code,
        message: err.to_string(),
    }
}

/// The real transport: HTTPS Web API calls and an outbound WebSocket.
pub struct HttpTransport {
    client: reqwest::Client,
    bot_token: Secret,
    app_token: Secret,
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("bot_token", &self.bot_token)
            .field("app_token", &self.app_token)
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    /// Reads both token files once (trimmed). The tokens go only into request headers.
    pub fn new(bot_token_file: &Path, app_token_file: &Path) -> Result<Self, AdapterError> {
        Ok(Self::from_secrets(
            read_secret(bot_token_file, E_SLACK_AUTH)?,
            read_secret(app_token_file, E_SLACK_APP_TOKEN)?,
        ))
    }

    fn from_secrets(bot_token: Secret, app_token: Secret) -> Self {
        Self {
            client: reqwest::Client::new(),
            bot_token,
            app_token,
        }
    }

    /// POSTs to a Web API method. `Ok(Err(wait))` is an HTTP 429 with its Retry-After.
    async fn call(
        &self,
        url: &str,
        token: &Secret,
        body: Option<&Value>,
        code: &'static str,
    ) -> Result<Result<Value, Duration>, AdapterError> {
        let request = self.client.post(url).bearer_auth(&token.0);
        let request = match body {
            Some(body) => request
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/json; charset=utf-8",
                )
                .body(body.to_string()),
            None => request.header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            ),
        };
        let response = request.send().await.map_err(|err| api_error(code, err))?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let wait = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map_or(RETRY_AFTER_DEFAULT, Duration::from_secs);
            return Ok(Err(wait.min(RETRY_AFTER_MAX)));
        }
        let json = response
            .json::<Value>()
            .await
            .map_err(|err| api_error(code, err))?;
        Ok(Ok(json))
    }
}

fn rate_limited(method: &str) -> AdapterError {
    AdapterError {
        code: E_SLACK_RATE,
        message: format!("{method} is rate limited"),
    }
}

#[async_trait]
impl SlackTransport for HttpTransport {
    async fn auth_test(&self) -> Result<String, AdapterError> {
        match self
            .call(AUTH_TEST_URL, &self.bot_token, None, E_SLACK_AUTH)
            .await?
        {
            Ok(json) => parse_auth_response(&json),
            Err(_) => Err(rate_limited("auth.test")),
        }
    }

    async fn open_connection(&self) -> Result<String, AdapterError> {
        match self
            .call(CONNECTIONS_OPEN_URL, &self.app_token, None, E_SLACK_API)
            .await?
        {
            Ok(json) => parse_open_response(&json),
            Err(_) => Err(rate_limited("apps.connections.open")),
        }
    }

    async fn connect(&self, url: &str) -> Result<Box<dyn SlackSocket>, AdapterError> {
        // The URL carries a connection ticket, so it never goes into an error message.
        let (stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|err| api_error(E_SLACK_API, format!("websocket connect failed: {err}")))?;
        Ok(Box::new(WsSocket { stream }))
    }

    async fn post_message(&self, body: Value) -> Result<PostOutcome, AdapterError> {
        match self
            .call(POST_MESSAGE_URL, &self.bot_token, Some(&body), E_SLACK_API)
            .await?
        {
            Ok(json) => parse_post_response(&json).map(PostOutcome::Posted),
            Err(wait) => Ok(PostOutcome::RateLimited(wait)),
        }
    }
}

struct WsSocket {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

#[async_trait]
impl SlackSocket for WsSocket {
    async fn next(&mut self) -> Option<Result<SocketMessage, AdapterError>> {
        loop {
            let message = match self.stream.next().await? {
                Ok(message) => message,
                Err(err) => return Some(Err(api_error(E_SLACK_API, err))),
            };
            // tungstenite queues the answer to a server ping and flushes it as we keep reading.
            return Some(Ok(match message {
                Message::Text(text) => SocketMessage::Text(text.to_string()),
                Message::Pong(_) | Message::Ping(_) => SocketMessage::Pong,
                Message::Close(_) => SocketMessage::Close,
                Message::Binary(_) | Message::Frame(_) => continue,
            }));
        }
    }

    async fn send_text(&mut self, text: String) -> Result<(), AdapterError> {
        self.stream
            .send(Message::text(text))
            .await
            .map_err(|err| api_error(E_SLACK_API, err))
    }

    async fn ping(&mut self) -> Result<(), AdapterError> {
        self.stream
            .send(Message::Ping(Default::default()))
            .await
            .map_err(|err| api_error(E_SLACK_API, err))
    }

    async fn close(&mut self) {
        let _ = self.stream.close(None).await;
    }
}

/// How one Socket Mode connection ended.
enum SessionEnd {
    Stopped,
    /// Slack asked us to reconnect (`disconnect`): open a new URL at once.
    Refresh,
    /// Retry only after the longest backoff (Socket Mode disabled, app token rejected).
    Hold,
    /// Error, close or silence; `lasted` is how long the connection was up.
    Failed {
        lasted: Duration,
    },
}

/// Resolves once shutdown was requested (or the adapter is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

struct SocketTask {
    transport: Arc<dyn SlackTransport>,
    scope: Scope,
    tx: mpsc::Sender<InboundEvent>,
    reconnects: watch::Sender<u64>,
    stop: watch::Receiver<bool>,
}

impl SocketTask {
    /// Keeps a Socket Mode connection up until shutdown.
    async fn run(mut self) {
        let mut delay = BACKOFF_START;
        loop {
            let wait = match self.session().await {
                SessionEnd::Stopped => return,
                SessionEnd::Refresh => Duration::ZERO,
                SessionEnd::Hold => BACKOFF_MAX,
                SessionEnd::Failed { lasted } => {
                    if lasted >= BACKOFF_RESET_AFTER {
                        delay = BACKOFF_START;
                    }
                    let wait = delay;
                    delay = next_backoff(delay, BACKOFF_START, BACKOFF_MAX);
                    wait
                }
            };
            if wait.is_zero() {
                continue;
            }
            tracing::info!(retry_in_secs = wait.as_secs(), "reconnecting to slack");
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = stopped(&mut self.stop) => return,
            }
        }
    }

    async fn session(&mut self) -> SessionEnd {
        let url = tokio::select! {
            result = self.transport.open_connection() => result,
            () = stopped(&mut self.stop) => return SessionEnd::Stopped,
        };
        let url = match url {
            Ok(url) => url,
            Err(err) if err.code == E_SLACK_APP_TOKEN => {
                tracing::error!(code = err.code, reason = %err.message, "slack rejected the app token");
                return SessionEnd::Hold;
            }
            Err(err) => {
                tracing::warn!(code = err.code, "apps.connections.open failed");
                return SessionEnd::Failed {
                    lasted: Duration::ZERO,
                };
            }
        };
        let socket = tokio::select! {
            result = self.transport.connect(&url) => result,
            () = stopped(&mut self.stop) => return SessionEnd::Stopped,
        };
        let mut socket = match socket {
            Ok(socket) => socket,
            Err(err) => {
                tracing::warn!(code = err.code, "slack websocket connect failed");
                return SessionEnd::Failed {
                    lasted: Duration::ZERO,
                };
            }
        };
        let end = self.read(socket.as_mut()).await;
        socket.close().await;
        end
    }

    async fn read(&mut self, socket: &mut dyn SlackSocket) -> SessionEnd {
        let started = Instant::now();
        let failed = || SessionEnd::Failed {
            lasted: started.elapsed(),
        };
        let mut ping = tokio::time::interval_at(started + PING_EVERY, PING_EVERY);
        let mut last_seen = started;
        loop {
            let message = tokio::select! {
                () = stopped(&mut self.stop) => return SessionEnd::Stopped,
                () = tokio::time::sleep_until(last_seen + SILENCE_LIMIT) => {
                    tracing::warn!(code = E_SLACK_PROTOCOL, "slack socket silent, reconnecting");
                    return failed();
                }
                _ = ping.tick() => {
                    if let Err(err) = socket.ping().await {
                        tracing::warn!(code = err.code, "slack ping failed");
                        return failed();
                    }
                    continue;
                }
                message = socket.next() => message,
            };
            last_seen = Instant::now();
            let text = match message {
                Some(Ok(SocketMessage::Text(text))) => text,
                Some(Ok(SocketMessage::Pong)) => continue,
                Some(Ok(SocketMessage::Close)) | None => {
                    tracing::info!("slack socket closed");
                    return failed();
                }
                Some(Err(err)) => {
                    tracing::warn!(code = err.code, "slack socket error");
                    return failed();
                }
            };
            let frame = match parse_frame(&text) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!(code = err.code, "unreadable slack frame skipped");
                    continue;
                }
            };
            match frame {
                Frame::Hello => {
                    tracing::info!("slack socket connected");
                    self.reconnects.send_modify(|count| *count += 1);
                }
                Frame::Disconnect { reason } if reason == DISCONNECT_LINK_DISABLED => {
                    tracing::error!(
                        code = E_SLACK_APP_TOKEN,
                        reason,
                        "slack socket mode is disabled"
                    );
                    return SessionEnd::Hold;
                }
                Frame::Disconnect { reason } => {
                    tracing::info!(reason, "slack asked for a reconnect");
                    return SessionEnd::Refresh;
                }
                Frame::Event {
                    envelope_id,
                    payload,
                } => {
                    if let Err(err) = socket.send_text(ack_frame(&envelope_id)).await {
                        tracing::warn!(code = err.code, "slack ack failed");
                        return failed();
                    }
                    if let Some(event) = map_event(&payload, &self.scope)
                        && self.tx.send(event).await.is_err()
                    {
                        tracing::debug!("inbound receiver closed, slack message dropped");
                    }
                }
                Frame::Other {
                    envelope_id: Some(envelope_id),
                } => {
                    if let Err(err) = socket.send_text(ack_frame(&envelope_id)).await {
                        tracing::warn!(code = err.code, "slack ack failed");
                        return failed();
                    }
                }
                Frame::Other { envelope_id: None } => {}
            }
        }
    }
}

#[derive(Default)]
struct Runtime {
    started: bool,
    task: Option<JoinHandle<()>>,
}

pub struct SlackAdapter {
    transport: Arc<dyn SlackTransport>,
    owner_id: String,
    channel_id: String,
    reconnects: watch::Sender<u64>,
    stop: watch::Sender<bool>,
    runtime: Mutex<Runtime>,
}

impl SlackAdapter {
    pub fn from_config(config: &Config) -> Result<Self, AdapterError> {
        let slack = config.slack.as_ref().ok_or_else(|| AdapterError {
            code: E_MESSENGER_MISSING,
            message: "the [slack] config section is missing".into(),
        })?;
        let transport = HttpTransport::new(&slack.bot_token_file, &slack.app_token_file)?;
        Ok(Self::with_transport(
            Arc::new(transport),
            slack.owner_id.clone(),
            slack.channel_id.clone(),
        ))
    }

    pub fn with_transport(
        transport: Arc<dyn SlackTransport>,
        owner_id: String,
        channel_id: String,
    ) -> Self {
        Self {
            transport,
            owner_id,
            channel_id,
            reconnects: watch::channel(0).0,
            stop: watch::channel(false).0,
            runtime: Mutex::new(Runtime::default()),
        }
    }
}

fn start_error(message: String) -> AdapterError {
    AdapterError {
        code: E_ADAPTER_START,
        message,
    }
}

fn send_error(err: &AdapterError) -> AdapterError {
    AdapterError {
        code: E_ADAPTER_SEND,
        message: err.to_string(),
    }
}

#[async_trait]
impl MessengerAdapter for SlackAdapter {
    fn platform(&self) -> &'static str {
        PLATFORM
    }

    fn message_limit(&self) -> usize {
        MESSAGE_LIMIT
    }

    fn behavior_hints(&self) -> &'static str {
        DEFAULT_HINTS
    }

    async fn start(&self) -> Result<mpsc::Receiver<InboundEvent>, AdapterError> {
        {
            let mut runtime = self.runtime.lock().expect("runtime lock");
            if runtime.started {
                return Err(start_error("the slack adapter was already started".into()));
            }
            runtime.started = true;
        }
        let bot_user_id = self
            .transport
            .auth_test()
            .await
            .map_err(|err| start_error(err.to_string()))?;
        let (tx, rx) = mpsc::channel(INBOUND_BUFFER);
        let task = SocketTask {
            transport: self.transport.clone(),
            scope: Scope {
                owner_id: self.owner_id.clone(),
                channel_id: self.channel_id.clone(),
                bot_user_id,
            },
            tx,
            reconnects: self.reconnects.clone(),
            stop: self.stop.subscribe(),
        };
        self.runtime.lock().expect("runtime lock").task = Some(tokio::spawn(task.run()));
        Ok(rx)
    }

    async fn send(&self, target: &str, text: &str) -> Result<SendResult, AdapterError> {
        let (channel, thread_ts) = parse_target(target).map_err(|err| send_error(&err))?;
        let body = post_body(&channel, thread_ts.as_deref(), &escape_text(text));
        for attempt in 1..=SEND_ATTEMPTS {
            match self.transport.post_message(body.clone()).await {
                Ok(PostOutcome::Posted(ts)) => {
                    return Ok(SendResult {
                        message_id: format!("{channel}:{ts}"),
                    });
                }
                Ok(PostOutcome::RateLimited(wait)) if attempt < SEND_ATTEMPTS => {
                    tokio::time::sleep(wait.min(RETRY_AFTER_MAX)).await;
                }
                Ok(PostOutcome::RateLimited(_)) => {}
                Err(err) => return Err(send_error(&err)),
            }
        }
        Err(send_error(&rate_limited("chat.postMessage")))
    }

    async fn create_thread(
        &self,
        _channel_id: &str,
        from_message_id: &str,
        _name: &str,
    ) -> Result<String, AdapterError> {
        // A Slack thread is just replies with `thread_ts`: the root message id is the thread.
        Ok(from_message_id.to_string())
    }

    async fn start_typing(&self, _channel: &str) -> Result<TypingHandle, AdapterError> {
        Ok(TypingHandle::new(|| {}))
    }

    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        Some(self.reconnects.subscribe())
    }

    async fn shutdown(&self) {
        self.stop.send_replace(true);
        let task = self.runtime.lock().expect("runtime lock").task.take();
        if let Some(task) = task
            && let Err(err) = task.await
        {
            tracing::warn!(error = %err, "slack socket task ended abnormally");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            owner_id: "U111".into(),
            channel_id: "C222".into(),
            bot_user_id: "UBOT".into(),
        }
    }

    fn payload(event: Value) -> Value {
        json!({ "type": "event_callback", "event": event })
    }

    fn msg(user: &str, channel: &str, ts: &str) -> Value {
        json!({ "type": "message", "user": user, "channel": channel, "ts": ts, "text": "hi",
                "channel_type": "channel" })
    }

    #[test]
    fn hello_and_disconnect_parse() {
        assert_eq!(
            parse_frame(r#"{"type":"hello","num_connections":1}"#).unwrap(),
            Frame::Hello
        );
        assert_eq!(
            parse_frame(r#"{"type":"disconnect","reason":"refresh_requested"}"#).unwrap(),
            Frame::Disconnect {
                reason: "refresh_requested".into()
            }
        );
        assert_eq!(
            parse_frame(r#"{"type":"slash_commands","envelope_id":"e9"}"#).unwrap(),
            Frame::Other {
                envelope_id: Some("e9".into())
            }
        );
    }

    #[test]
    fn events_api_envelope_parses_and_acks() {
        let frame = parse_frame(
            r#"{"type":"events_api","envelope_id":"e1","payload":{"type":"event_callback"}}"#,
        )
        .unwrap();
        let Frame::Event {
            envelope_id,
            payload,
        } = frame
        else {
            panic!("expected event frame");
        };
        assert_eq!(envelope_id, "e1");
        assert_eq!(payload["type"], "event_callback");
        let ack: Value = serde_json::from_str(&ack_frame(&envelope_id)).unwrap();
        assert_eq!(ack, json!({ "envelope_id": "e1" }));
    }

    #[test]
    fn bad_json_is_protocol_error() {
        assert_eq!(parse_frame("not json").unwrap_err().code, E_SLACK_PROTOCOL);
    }

    #[test]
    fn owner_channel_message_maps_without_thread() {
        let event = map_event(&payload(msg("U111", "C222", "1.1")), &scope()).unwrap();
        assert_eq!(event.message_id, "C222:1.1");
        assert_eq!(event.author_id, "U111");
        assert!(!event.author_is_bot);
        assert_eq!(event.channel_id, "C222");
        assert_eq!(event.thread_id, None);
        assert!(!event.is_dm);
        assert_eq!(event.text, "hi");
    }

    #[test]
    fn thread_reply_maps_thread_id() {
        let mut m = msg("U111", "C222", "2.2");
        m["thread_ts"] = json!("1.1");
        let event = map_event(&payload(m), &scope()).unwrap();
        assert_eq!(event.thread_id.as_deref(), Some("C222:1.1"));
    }

    #[test]
    fn thread_root_with_equal_thread_ts_has_no_thread() {
        let mut m = msg("U111", "C222", "1.1");
        m["thread_ts"] = json!("1.1");
        assert_eq!(map_event(&payload(m), &scope()).unwrap().thread_id, None);
    }

    #[test]
    fn owner_dm_maps_is_dm() {
        let mut m = msg("U111", "D333", "3.3");
        m["channel_type"] = json!("im");
        let event = map_event(&payload(m), &scope()).unwrap();
        assert!(event.is_dm);
        assert_eq!(event.thread_id, None);
        assert_eq!(event.channel_id, "D333");
    }

    #[test]
    fn other_user_dm_dropped() {
        let mut m = msg("U999", "D333", "3.3");
        m["channel_type"] = json!("im");
        assert!(map_event(&payload(m), &scope()).is_none());
    }

    #[test]
    fn other_channel_dropped() {
        assert!(map_event(&payload(msg("U111", "C777", "1.1")), &scope()).is_none());
    }

    #[test]
    fn missing_channel_dropped() {
        let mut m = msg("U111", "C222", "1.1");
        m.as_object_mut().unwrap().remove("channel");
        assert!(map_event(&payload(m), &scope()).is_none());
    }

    #[test]
    fn bot_id_dropped() {
        let mut m = msg("U111", "C222", "1.1");
        m["bot_id"] = json!("B1");
        assert!(map_event(&payload(m), &scope()).is_none());
    }

    #[test]
    fn own_bot_user_dropped() {
        assert!(map_event(&payload(msg("UBOT", "C222", "1.1")), &scope()).is_none());
    }

    #[test]
    fn subtype_message_changed_dropped() {
        let mut m = msg("U111", "C222", "1.1");
        m["subtype"] = json!("message_changed");
        assert!(map_event(&payload(m), &scope()).is_none());
    }

    #[test]
    fn thread_broadcast_kept() {
        let mut m = msg("U111", "C222", "2.2");
        m["subtype"] = json!("thread_broadcast");
        m["thread_ts"] = json!("1.1");
        assert!(map_event(&payload(m), &scope()).is_some());
    }

    #[test]
    fn leading_bot_mention_stripped() {
        let mut m = msg("U111", "C222", "1.1");
        m["text"] = json!("<@UBOT>  run it <@UBOT>");
        let event = map_event(&payload(m), &scope()).unwrap();
        assert_eq!(event.text, "run it <@UBOT>");
    }

    #[test]
    fn non_message_payloads_dropped() {
        let reaction = json!({ "type": "reaction_added", "user": "U111" });
        assert!(map_event(&payload(reaction), &scope()).is_none());
        assert!(map_event(&json!({ "type": "url_verification" }), &scope()).is_none());
    }

    #[test]
    fn parse_target_thread_and_plain() {
        assert_eq!(
            parse_target("C222:1.1").unwrap(),
            ("C222".to_string(), Some("1.1".to_string()))
        );
        assert_eq!(parse_target("D333").unwrap(), ("D333".to_string(), None));
        for bad in ["", ":1.1", "C222:"] {
            assert_eq!(parse_target(bad).unwrap_err().code, E_SLACK_ID);
        }
    }

    #[test]
    fn escape_only_three_chars() {
        assert_eq!(
            escape_text("a & <b> \"c\" 'd'"),
            "a &amp; &lt;b&gt; \"c\" 'd'"
        );
    }

    #[test]
    fn post_body_has_thread_ts_only_when_given() {
        assert_eq!(
            post_body("C222", Some("1.1"), "x"),
            json!({ "channel": "C222", "text": "x", "thread_ts": "1.1" })
        );
        assert_eq!(
            post_body("D333", None, "x"),
            json!({ "channel": "D333", "text": "x" })
        );
    }

    #[test]
    fn open_response_ok_and_error() {
        assert_eq!(
            parse_open_response(&json!({ "ok": true, "url": "wss://example.test/link" })).unwrap(),
            "wss://example.test/link"
        );
        let err =
            parse_open_response(&json!({ "ok": false, "error": "invalid_auth" })).unwrap_err();
        assert_eq!(err.code, E_SLACK_APP_TOKEN);
        assert_eq!(err.message, "invalid_auth");
        let err = parse_open_response(&json!({ "ok": true })).unwrap_err();
        assert_eq!(err.code, E_SLACK_PROTOCOL);
    }

    #[test]
    fn auth_response_error_code() {
        assert_eq!(
            parse_auth_response(&json!({ "ok": true, "user_id": "UBOT" })).unwrap(),
            "UBOT"
        );
        let err =
            parse_auth_response(&json!({ "ok": false, "error": "invalid_auth" })).unwrap_err();
        assert_eq!(err.code, E_SLACK_AUTH);
    }

    #[test]
    fn post_response_ts_and_error_code() {
        assert_eq!(
            parse_post_response(&json!({ "ok": true, "channel": "C222", "ts": "9.9" })).unwrap(),
            "9.9"
        );
        let err =
            parse_post_response(&json!({ "ok": false, "error": "channel_not_found" })).unwrap_err();
        assert_eq!(err.code, E_SLACK_API);
    }

    #[test]
    fn http_transport_debug_has_no_token() {
        let transport = HttpTransport::from_secrets(
            Secret("xoxb-test-SENTINEL".into()),
            Secret("xapp-test-SENTINEL".into()),
        );
        let debug = format!("{transport:?}");
        assert!(debug.contains("HttpTransport"));
        assert!(!debug.contains("SENTINEL"));
    }

    #[test]
    fn token_files_are_read_trimmed_and_empty_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bot = dir.path().join("bot");
        let app = dir.path().join("app");
        std::fs::write(&bot, "xoxb-test-111\n").unwrap();
        std::fs::write(&app, "  \n").unwrap();
        assert_eq!(read_secret(&bot, E_SLACK_AUTH).unwrap().0, "xoxb-test-111");
        assert_eq!(
            HttpTransport::new(&bot, &app).unwrap_err().code,
            E_SLACK_APP_TOKEN
        );
        assert_eq!(
            HttpTransport::new(&dir.path().join("missing"), &app)
                .unwrap_err()
                .code,
            E_SLACK_AUTH
        );
    }
}
