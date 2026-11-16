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
    AdapterError, AttachmentRef, E_ADAPTER_SEND, E_ADAPTER_START, E_ADAPTER_UPLOAD_DENIED,
    InboundAttachment, InboundEvent, MessengerAdapter, SendResult, TypingHandle, next_backoff,
};
use crate::config::{Config, E_MESSENGER_MISSING};
use crate::files::{self, E_FILE_REMOTE, Progress};

pub const PLATFORM: &str = "slack";
/// Slack advises keeping `text` under 4,000 characters.
pub const MESSAGE_LIMIT: usize = 3900;

pub const E_SLACK_AUTH: &str = "E_SLACK_AUTH";
pub const E_SLACK_APP_TOKEN: &str = "E_SLACK_APP_TOKEN";
pub const E_SLACK_API: &str = "E_SLACK_API";
pub const E_SLACK_RATE: &str = "E_SLACK_RATE";
pub const E_SLACK_ID: &str = "E_SLACK_ID";
pub const E_SLACK_PROTOCOL: &str = "E_SLACK_PROTOCOL";
/// A file upload step failed in a way that may work next time (network, 5xx, 429, or a
/// transient Slack error).
pub const E_SLACK_UPLOAD: &str = "E_SLACK_UPLOAD";

/// The only host Slack serves file downloads and upload URLs from.
pub const FILE_HOST: &str = "files.slack.com";

pub const DEFAULT_HINTS: &str = "Your replies are posted to Slack. Use Slack mrkdwn, not Markdown: *bold*, _italic_, ~strike~, `code`, fenced code blocks, > quotes, - lists. Double-asterisk bold, # headings and tables do not render. Write links as plain URLs. One message holds about 3900 characters; longer replies are split. Files the owner shares in Slack are saved on disk; their paths are listed at the end of the message. To send a file back into the Slack thread or DM, call the send_file tool with a path inside the work folder (at most 10 files per reply).";

/// Slack `error` strings on a file call that may succeed when retried.
const TRANSIENT_UPLOAD_ERRORS: [&str; 5] = [
    "ratelimited",
    "internal_error",
    "fatal_error",
    "service_unavailable",
    "request_timeout",
];

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

/// Maps one `files[]` entry. Never fails: a missing field gets a neutral value, and a file
/// Slack cannot serve (external, remote, access stub, hidden, deleted, no download URL)
/// gets an empty reference, which the adapter's fetch refuses with `E_FILE_REMOTE`.
fn map_file(entry: &Value) -> InboundAttachment {
    let str_field = |key: &str| entry.get(key).and_then(Value::as_str);
    let unservable = matches!(
        str_field("mode"),
        Some("external" | "file_access" | "hidden_by_limit" | "tombstone")
    ) || entry.get("is_external").and_then(Value::as_bool) == Some(true)
        || str_field("filetype") == Some("remote");
    let reference = if unservable {
        String::new()
    } else {
        str_field("url_private_download")
            .unwrap_or_default()
            .to_string()
    };
    InboundAttachment {
        name: str_field("name").unwrap_or("file").to_string(),
        size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
        mime: str_field("mimetype").map(str::to_string),
        reference: AttachmentRef(reference),
    }
}

/// Maps an `events_api` payload to an inbound event, or `None` when the message must be
/// dropped: wrong type, bot or own messages, edits and subtypes other than `file_share`
/// and `thread_broadcast`, other users in a DM, other channels. Every field is optional on
/// the wire; a missing one drops the event (an odd `files[]` entry never does).
pub fn map_event(payload: &Value, scope: &Scope) -> Option<InboundEvent> {
    if payload.get("type")?.as_str()? != "event_callback" {
        return None;
    }
    let event = payload.get("event")?;
    if event.get("type")?.as_str()? != "message" {
        return None;
    }
    if let Some(subtype) = event.get("subtype") {
        if !matches!(subtype.as_str(), Some("thread_broadcast" | "file_share")) {
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
    let attachments = event
        .get("files")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().map(map_file).collect())
        .unwrap_or_default();
    Some(InboundEvent {
        message_id: format!("{channel}:{ts}"),
        author_id: user.to_string(),
        author_is_bot: false,
        channel_id: channel.to_string(),
        thread_id,
        is_dm,
        text: text.to_string(),
        attachments,
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

/// Classifies Slack's `error` string from a file call: the transient ones may work next
/// time (`E_SLACK_UPLOAD`); every other one, `missing_scope` and size refusals included,
/// is permanent (`E_ADAPTER_UPLOAD_DENIED`).
pub fn upload_error_code(slack_error: &str) -> &'static str {
    if TRANSIENT_UPLOAD_ERRORS.contains(&slack_error) {
        E_SLACK_UPLOAD
    } else {
        E_ADAPTER_UPLOAD_DENIED
    }
}

/// Classifies a non-200 HTTP status of the upload POST: 4xx other than 429 (413 too large
/// included) is permanent, anything else may work next time.
pub fn upload_status_code(status: u16) -> &'static str {
    if (400..500).contains(&status) && status != 429 {
        E_ADAPTER_UPLOAD_DENIED
    } else {
        E_SLACK_UPLOAD
    }
}

/// `ok: false` on a file call -> its error, classified by `upload_error_code`.
fn upload_ok(json: &Value) -> Result<(), AdapterError> {
    if json.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    let reason = json
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    Err(AdapterError {
        code: upload_error_code(reason),
        message: reason.to_string(),
    })
}

/// `files.getUploadURLExternal` response -> `(upload_url, file_id)`.
pub fn parse_upload_url_response(json: &Value) -> Result<(String, String), AdapterError> {
    upload_ok(json)?;
    let field = |key: &str| {
        json.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| protocol_error(format!("response without {key}")))
    };
    Ok((field("upload_url")?, field("file_id")?))
}

/// The `files.completeUploadExternal` body: shares the uploaded file into the channel, or
/// into the thread when `thread_ts` is given.
pub fn complete_upload_body(
    file_id: &str,
    title: &str,
    channel_id: &str,
    thread_ts: Option<&str>,
) -> Value {
    let mut body = json!({
        "files": [{ "id": file_id, "title": title }],
        "channel_id": channel_id,
    });
    if let Some(thread_ts) = thread_ts {
        body["thread_ts"] = json!(thread_ts);
    }
    body
}

const AUTH_TEST_URL: &str = "https://slack.com/api/auth.test";
const CONNECTIONS_OPEN_URL: &str = "https://slack.com/api/apps.connections.open";
const POST_MESSAGE_URL: &str = "https://slack.com/api/chat.postMessage";
const UPLOAD_URL_URL: &str = "https://slack.com/api/files.getUploadURLExternal";
const COMPLETE_UPLOAD_URL: &str = "https://slack.com/api/files.completeUploadExternal";

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
    /// Streams a `url_private_download` file into the new file `dest` with the bot token;
    /// only `FILE_HOST` is allowed and redirects are not followed. `reject_html` refuses a
    /// 2xx `text/html` answer (Slack's login page) with `E_FILE_DENIED`.
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        reject_html: bool,
        progress: &Progress,
    ) -> Result<u64, AdapterError>;
    /// `files.getUploadURLExternal` -> `(upload_url, file_id)`.
    async fn upload_url(
        &self,
        filename: &str,
        length: u64,
    ) -> Result<(String, String), AdapterError>;
    /// One POST of the whole file to an upload URL the caller has host-checked; no
    /// `Authorization` header.
    async fn upload_bytes(&self, url: reqwest::Url, bytes: Vec<u8>) -> Result<(), AdapterError>;
    /// `files.completeUploadExternal`: shares the uploaded file into the channel or thread.
    async fn complete_upload(
        &self,
        file_id: &str,
        title: &str,
        channel_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), AdapterError>;
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

/// A Web API request body.
enum ApiBody<'a> {
    Empty,
    Json(&'a Value),
    Form(&'a [(&'a str, String)]),
}

/// The real transport: HTTPS Web API calls and an outbound WebSocket.
pub struct HttpTransport {
    client: reqwest::Client,
    /// `files::file_client()`: downloads and the upload POST only.
    file_client: reqwest::Client,
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
            file_client: files::file_client(),
            bot_token,
            app_token,
        }
    }

    /// POSTs to a Web API method. `Ok(Err(wait))` is an HTTP 429 with its Retry-After.
    async fn call(
        &self,
        url: &str,
        token: &Secret,
        body: ApiBody<'_>,
        code: &'static str,
    ) -> Result<Result<Value, Duration>, AdapterError> {
        let request = self.client.post(url).bearer_auth(&token.0);
        let request = match body {
            ApiBody::Json(body) => request
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/json; charset=utf-8",
                )
                .body(body.to_string()),
            ApiBody::Form(fields) => request.form(fields),
            ApiBody::Empty => request.header(
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
            .call(AUTH_TEST_URL, &self.bot_token, ApiBody::Empty, E_SLACK_AUTH)
            .await?
        {
            Ok(json) => parse_auth_response(&json),
            Err(_) => Err(rate_limited("auth.test")),
        }
    }

    async fn open_connection(&self) -> Result<String, AdapterError> {
        match self
            .call(
                CONNECTIONS_OPEN_URL,
                &self.app_token,
                ApiBody::Empty,
                E_SLACK_API,
            )
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
            .call(
                POST_MESSAGE_URL,
                &self.bot_token,
                ApiBody::Json(&body),
                E_SLACK_API,
            )
            .await?
        {
            Ok(json) => parse_post_response(&json).map(PostOutcome::Posted),
            Err(wait) => Ok(PostOutcome::RateLimited(wait)),
        }
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        reject_html: bool,
        progress: &Progress,
    ) -> Result<u64, AdapterError> {
        let url = files::check_url(url, &[FILE_HOST])?;
        files::fetch_to_file(
            &self.file_client,
            url,
            Some(&self.bot_token.0),
            dest,
            reject_html,
            progress,
        )
        .await
    }

    async fn upload_url(
        &self,
        filename: &str,
        length: u64,
    ) -> Result<(String, String), AdapterError> {
        let fields = [
            ("filename", filename.to_string()),
            ("length", length.to_string()),
        ];
        match self
            .call(
                UPLOAD_URL_URL,
                &self.bot_token,
                ApiBody::Form(&fields),
                E_SLACK_UPLOAD,
            )
            .await?
        {
            Ok(json) => parse_upload_url_response(&json),
            Err(_) => Err(upload_rate_limited("files.getUploadURLExternal")),
        }
    }

    async fn upload_bytes(&self, url: reqwest::Url, bytes: Vec<u8>) -> Result<(), AdapterError> {
        match files::post_bytes(&self.file_client, url, bytes).await {
            Some(200) => Ok(()),
            Some(status) => Err(AdapterError {
                code: upload_status_code(status),
                message: format!("the file upload answered HTTP {status}"),
            }),
            None => Err(AdapterError {
                code: E_SLACK_UPLOAD,
                message: "the file upload failed on the network".to_string(),
            }),
        }
    }

    async fn complete_upload(
        &self,
        file_id: &str,
        title: &str,
        channel_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), AdapterError> {
        let body = complete_upload_body(file_id, title, channel_id, thread_ts);
        match self
            .call(
                COMPLETE_UPLOAD_URL,
                &self.bot_token,
                ApiBody::Json(&body),
                E_SLACK_UPLOAD,
            )
            .await?
        {
            Ok(json) => upload_ok(&json),
            Err(_) => Err(upload_rate_limited("files.completeUploadExternal")),
        }
    }
}

fn upload_rate_limited(method: &str) -> AdapterError {
    AdapterError {
        code: E_SLACK_UPLOAD,
        message: format!("{method} is rate limited"),
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

    async fn fetch_attachment(
        &self,
        attachment: &InboundAttachment,
        dest: &Path,
        progress: &Progress,
    ) -> Result<u64, AdapterError> {
        if attachment.reference.0.is_empty() {
            return Err(AdapterError {
                code: E_FILE_REMOTE,
                message: "slack cannot serve this file for download".to_string(),
            });
        }
        // Without `files:read` Slack answers 200 with its HTML login page.
        let reject_html = !attachment
            .mime
            .as_deref()
            .is_some_and(|mime| mime.starts_with("text/html"));
        self.transport
            .download(&attachment.reference.0, dest, reject_html, progress)
            .await
    }

    /// Reads the file whole, then gets an upload URL, POSTs the bytes and shares the file
    /// into the target channel or thread. Returns the Slack file id.
    async fn send_file(
        &self,
        target: &str,
        path: &Path,
        name: &str,
    ) -> Result<SendResult, AdapterError> {
        let (channel, thread_ts) = parse_target(target)?;
        let data = tokio::fs::read(path).await.map_err(|err| AdapterError {
            code: E_ADAPTER_UPLOAD_DENIED,
            message: format!("the file could not be read: {:?}", err.kind()),
        })?;
        let (upload_url, file_id) = self.transport.upload_url(name, data.len() as u64).await?;
        let url = files::check_url(&upload_url, &[FILE_HOST]).map_err(|_| AdapterError {
            code: E_SLACK_UPLOAD,
            message: "slack returned an upload URL on an unexpected host".to_string(),
        })?;
        self.transport.upload_bytes(url, data).await?;
        self.transport
            .complete_upload(&file_id, name, &channel, thread_ts.as_deref())
            .await?;
        Ok(SendResult {
            message_id: file_id,
        })
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

    const DOWNLOAD_URL: &str = "https://files.slack.com/files-pri/T111-F111/download/report.pdf";

    fn pdf_file() -> Value {
        json!({ "id": "F111", "name": "report.pdf", "size": 1234, "mimetype": "application/pdf",
                "filetype": "pdf", "mode": "hosted", "url_private_download": DOWNLOAD_URL })
    }

    fn with_files(mut m: Value, files: Vec<Value>) -> Value {
        m["files"] = json!(files);
        m
    }

    fn file_share(user: &str, channel: &str, files: Vec<Value>) -> Value {
        let mut m = with_files(msg(user, channel, "1.1"), files);
        m["subtype"] = json!("file_share");
        m
    }

    fn only_attachment(m: Value) -> InboundAttachment {
        let event = map_event(&payload(m), &scope()).expect("event kept");
        assert_eq!(event.attachments.len(), 1);
        event.attachments.into_iter().next().unwrap()
    }

    #[test]
    fn file_share_maps_attachments() {
        let m = file_share("U111", "C222", vec![pdf_file()]);
        let event = map_event(&payload(m), &scope()).unwrap();
        assert_eq!(event.text, "hi");
        assert_eq!(
            event.attachments,
            vec![InboundAttachment {
                name: "report.pdf".into(),
                size: 1234,
                mime: Some("application/pdf".into()),
                reference: AttachmentRef(DOWNLOAD_URL.into()),
            }]
        );
    }

    #[test]
    fn file_share_without_text_maps() {
        let mut m = file_share("U111", "C222", vec![pdf_file(), pdf_file()]);
        m.as_object_mut().unwrap().remove("text");
        let event = map_event(&payload(m), &scope()).unwrap();
        assert_eq!(event.text, "");
        assert_eq!(event.attachments.len(), 2);
    }

    #[test]
    fn plain_message_with_files_maps_attachments() {
        let m = with_files(msg("U111", "C222", "1.1"), vec![pdf_file()]);
        assert_eq!(only_attachment(m).reference.0, DOWNLOAD_URL);
        let mut broadcast = with_files(msg("U111", "C222", "2.2"), vec![pdf_file()]);
        broadcast["subtype"] = json!("thread_broadcast");
        broadcast["thread_ts"] = json!("1.1");
        assert_eq!(only_attachment(broadcast).reference.0, DOWNLOAD_URL);
    }

    #[test]
    fn remote_file_has_empty_reference() {
        let variants = [
            ("mode", json!("external")),
            ("is_external", json!(true)),
            ("filetype", json!("remote")),
        ];
        for (key, value) in variants {
            let mut file = pdf_file();
            file[key] = value;
            let attachment = only_attachment(file_share("U111", "C222", vec![file]));
            assert_eq!(attachment.reference.0, "", "{key}");
            assert_eq!(attachment.name, "report.pdf");
        }
    }

    #[test]
    fn file_access_stub_has_empty_reference() {
        let stub = json!({ "id": "F111", "mode": "file_access", "file_access": "check_file_info",
                           "url_private_download": DOWNLOAD_URL });
        let attachment = only_attachment(file_share("U111", "C222", vec![stub]));
        assert_eq!(attachment.reference.0, "");
        assert_eq!(attachment.name, "file");
    }

    #[test]
    fn hidden_by_limit_file_has_empty_reference() {
        for mode in ["hidden_by_limit", "tombstone"] {
            let mut file = pdf_file();
            file["mode"] = json!(mode);
            let attachment = only_attachment(file_share("U111", "C222", vec![file]));
            assert_eq!(attachment.reference.0, "", "{mode}");
        }
    }

    #[test]
    fn file_without_download_url_has_empty_reference() {
        let mut missing = pdf_file();
        missing
            .as_object_mut()
            .unwrap()
            .remove("url_private_download");
        let mut not_string = pdf_file();
        not_string["url_private_download"] = json!(42);
        let m = file_share("U111", "C222", vec![missing, not_string]);
        let event = map_event(&payload(m), &scope()).unwrap();
        assert!(event.attachments.iter().all(|a| a.reference.0.is_empty()));
        assert_eq!(event.attachments.len(), 2);
    }

    #[test]
    fn file_entry_missing_name_and_size_still_maps() {
        let m = file_share("U111", "C222", vec![json!({ "id": "F111" }), json!("odd")]);
        let event = map_event(&payload(m), &scope()).unwrap();
        assert_eq!(event.text, "hi");
        assert_eq!(event.attachments.len(), 2);
        for attachment in &event.attachments {
            assert_eq!(attachment.name, "file");
            assert_eq!(attachment.size, 0);
            assert_eq!(attachment.mime, None);
            assert_eq!(attachment.reference.0, "");
        }
    }

    #[test]
    fn message_changed_still_dropped() {
        let mut m = with_files(msg("U111", "C222", "1.1"), vec![pdf_file()]);
        m["subtype"] = json!("message_changed");
        assert!(map_event(&payload(m), &scope()).is_none());
        let mut deleted = msg("U111", "C222", "1.1");
        deleted["subtype"] = json!("message_deleted");
        assert!(map_event(&payload(deleted), &scope()).is_none());
    }

    #[test]
    fn other_user_file_share_dropped() {
        let mut dm = file_share("U999", "D333", vec![pdf_file()]);
        dm["channel_type"] = json!("im");
        assert!(map_event(&payload(dm), &scope()).is_none());
        let other_channel = file_share("U111", "C777", vec![pdf_file()]);
        assert!(map_event(&payload(other_channel), &scope()).is_none());
        let mut bot = file_share("U111", "C222", vec![pdf_file()]);
        bot["bot_id"] = json!("B1");
        assert!(map_event(&payload(bot), &scope()).is_none());
    }

    #[test]
    fn default_hints_mention_send_file() {
        assert!(DEFAULT_HINTS.contains("send_file"));
    }

    #[test]
    fn upload_error_classification() {
        for error in [
            "missing_scope",
            "file_too_large",
            "not_in_channel",
            "invalid_auth",
        ] {
            assert_eq!(upload_error_code(error), E_ADAPTER_UPLOAD_DENIED, "{error}");
        }
        for error in TRANSIENT_UPLOAD_ERRORS {
            assert_eq!(upload_error_code(error), E_SLACK_UPLOAD, "{error}");
        }
        for status in [400, 403, 404, 413] {
            assert_eq!(
                upload_status_code(status),
                E_ADAPTER_UPLOAD_DENIED,
                "{status}"
            );
        }
        for status in [429, 500, 503] {
            assert_eq!(upload_status_code(status), E_SLACK_UPLOAD, "{status}");
        }
    }

    #[test]
    fn upload_url_response_parses_and_classifies() {
        assert_eq!(
            parse_upload_url_response(&json!({ "ok": true, "upload_url": "https://files.slack.com/upload/v1/x", "file_id": "F111" }))
                .unwrap(),
            ("https://files.slack.com/upload/v1/x".to_string(), "F111".to_string())
        );
        let err = parse_upload_url_response(&json!({ "ok": false, "error": "missing_scope" }))
            .unwrap_err();
        assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);
        let err =
            parse_upload_url_response(&json!({ "ok": false, "error": "ratelimited" })).unwrap_err();
        assert_eq!(err.code, E_SLACK_UPLOAD);
        let err = parse_upload_url_response(&json!({ "ok": true, "file_id": "F111" })).unwrap_err();
        assert_eq!(err.code, E_SLACK_PROTOCOL);
    }

    #[test]
    fn complete_upload_body_has_thread_ts_only_when_given() {
        assert_eq!(
            complete_upload_body("F111", "a.txt", "C222", Some("1.1")),
            json!({ "files": [{ "id": "F111", "title": "a.txt" }], "channel_id": "C222", "thread_ts": "1.1" })
        );
        assert_eq!(
            complete_upload_body("F111", "a.txt", "D333", None),
            json!({ "files": [{ "id": "F111", "title": "a.txt" }], "channel_id": "D333" })
        );
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
