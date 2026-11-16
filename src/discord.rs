//! Discord adapter on serenity 0.12.5.
//!
//! Only three kinds of messages become events: messages posted directly in the configured
//! channel, messages in threads whose parent is that channel, and DMs from the owner.
//! Everything else is dropped here without being stored. The owner check for channel and
//! thread messages stays in the core. The decisions are pure functions over plain fields
//! (`map_message`, `thread_name`, `reply_chunks`, ...) so they can be tested without Discord.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serenity::all::{
    Channel, ChannelId, ChannelType, Client, Context, CreateAttachment, CreateMessage,
    CreateThread, EventHandler, GatewayIntents, Http, Message, MessageId, MessageType, Ready,
    ResumedEvent, ShardManager,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::adapter::{
    AdapterError, AttachmentRef, E_ADAPTER_SEND, E_ADAPTER_START, E_ADAPTER_THREAD,
    E_ADAPTER_UPLOAD, E_ADAPTER_UPLOAD_DENIED, InboundAttachment, InboundEvent, MessengerAdapter,
    SendResult, TypingHandle, next_backoff, split_reply,
};
use crate::config::{Config, E_MESSENGER_MISSING};
use crate::files::{self, Progress};

pub const PLATFORM: &str = "discord";
/// Discord's message length limit, counted in characters.
pub const MESSAGE_LIMIT: usize = 2000;
pub const THREAD_NAME_MAX_CHARS: usize = 50;
pub const THREAD_NAME_MIN_CHARS: usize = 2;
pub const THREAD_NAME_FALLBACK: &str = "moraebot";
/// Sent instead of an empty reply, which Discord would reject.
pub const EMPTY_REPLY: &str = "(empty reply)";
/// Built-in behavior hints for omp; `<home>/hints/discord.md` overrides them.
pub const DEFAULT_HINTS: &str = "Your replies are posted to Discord. One message holds at most 2000 characters; longer replies are split into several messages. Discord markdown works: **bold**, *italic*, `code`, fenced code blocks, > quotes, - lists. Tables do not render, use lists. Keep answers short. Files the owner attaches are saved on disk; their paths are listed at the end of the message. To send a file back, call the send_file tool with a path inside the work folder (at most 10 files per reply).";
/// The only hosts attachments are downloaded from (exact match, https).
pub const DOWNLOAD_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];

pub const E_DISCORD_TOKEN: &str = "E_DISCORD_TOKEN";
pub const E_DISCORD_ID: &str = "E_DISCORD_ID";
pub const E_DISCORD_GATEWAY: &str = "E_DISCORD_GATEWAY";
pub const E_DISCORD_LOOKUP: &str = "E_DISCORD_LOOKUP";

const INBOUND_BUFFER: usize = 64;
const BACKOFF_START: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// A gateway session that lasted at least this long resets the reconnect backoff.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub owner_id: u64,
    pub channel_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Dm,
    GuildChannel,
    Thread { parent_id: Option<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessage<'a> {
    pub message_id: u64,
    pub author_id: u64,
    pub author_is_bot: bool,
    pub from_self: bool,
    /// A regular message or an inline reply; system messages are false.
    pub is_regular: bool,
    pub channel_id: u64,
    pub location: Location,
    pub text: &'a str,
    pub attachments: &'a [AttachmentMeta],
}

/// The fields of a Discord attachment moraebot uses. `url` is signed, so it is kept as an
/// `AttachmentRef`, whose `Debug` is redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    pub filename: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub url: AttachmentRef,
}

/// Turns a message into a core event, or `None` when it is outside the listening scope.
/// Empty text is kept as an empty `text`: with attachments it is a files-only message, without
/// them the core answers it with its fixed notice.
pub fn map_message(raw: &RawMessage<'_>, scope: &Scope) -> Option<InboundEvent> {
    if raw.from_self || !raw.is_regular {
        return None;
    }
    let (channel_id, thread_id, is_dm) = match raw.location {
        Location::Dm if raw.author_id == scope.owner_id => (raw.channel_id, None, true),
        Location::Dm => return None,
        Location::GuildChannel if raw.channel_id == scope.channel_id => {
            (raw.channel_id, None, false)
        }
        Location::GuildChannel => return None,
        Location::Thread {
            parent_id: Some(parent),
        } if parent == scope.channel_id => (parent, Some(raw.channel_id.to_string()), false),
        Location::Thread { .. } => return None,
    };
    Some(InboundEvent {
        message_id: raw.message_id.to_string(),
        author_id: raw.author_id.to_string(),
        author_is_bot: raw.author_is_bot,
        channel_id: channel_id.to_string(),
        thread_id,
        is_dm,
        text: raw.text.to_owned(),
        attachments: raw
            .attachments
            .iter()
            .map(|meta| InboundAttachment {
                name: meta.filename.clone(),
                size: meta.size,
                mime: meta.content_type.clone(),
                reference: meta.url.clone(),
            })
            .collect(),
    })
}

/// The adapter error code for a failed upload: a 4xx other than 429 (missing permission,
/// file too large for Discord) is a permanent refusal; 429, 5xx and network errors (`None`)
/// may work next time.
pub fn upload_error_code(status: Option<u16>) -> &'static str {
    match status {
        Some(status) if (400..500).contains(&status) && status != 429 => E_ADAPTER_UPLOAD_DENIED,
        _ => E_ADAPTER_UPLOAD,
    }
}

/// Reads `path` whole and posts it as one attachment into `channel_id` through serenity,
/// which sends the token and waits out rate limits. Returns the new message id.
async fn upload_file(
    http: &Http,
    channel_id: u64,
    path: &Path,
    name: &str,
) -> Result<String, AdapterError> {
    let data = tokio::fs::read(path).await.map_err(|err| AdapterError {
        code: E_ADAPTER_UPLOAD_DENIED,
        message: format!("cannot read the file ({:?})", err.kind()),
    })?;
    let message = ChannelId::new(channel_id)
        .send_files(
            http,
            [CreateAttachment::bytes(data, name)],
            CreateMessage::new(),
        )
        .await
        .map_err(|err| {
            let status = match &err {
                serenity::Error::Http(http_err) => http_err.status_code().map(|s| s.as_u16()),
                _ => None,
            };
            AdapterError {
                code: upload_error_code(status),
                message: match status {
                    Some(status) => format!("upload failed with HTTP {status}"),
                    None => "upload failed".to_owned(),
                },
            }
        })?;
    Ok(message.id.get().to_string())
}

pub fn guild_location(is_thread: bool, parent_id: Option<u64>) -> Location {
    if is_thread {
        Location::Thread { parent_id }
    } else {
        Location::GuildChannel
    }
}

/// Thread name from message text: whitespace collapsed, at most 50 characters, and the
/// fallback when fewer than 2 characters remain.
pub fn thread_name(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let name: String = collapsed.chars().take(THREAD_NAME_MAX_CHARS).collect();
    let name = name.trim_end();
    if name.chars().count() < THREAD_NAME_MIN_CHARS {
        THREAD_NAME_FALLBACK.to_owned()
    } else {
        name.to_owned()
    }
}

/// The messages to post for `text`, each within the Discord limit and never blank.
pub fn reply_chunks(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return vec![EMPTY_REPLY.to_owned()];
    }
    split_reply(text, MESSAGE_LIMIT)
        .into_iter()
        .filter(|chunk| !chunk.trim().is_empty())
        .collect()
}

pub fn parse_id(id: &str) -> Result<u64, AdapterError> {
    match id.parse::<u64>() {
        Ok(value) if value != 0 => Ok(value),
        _ => Err(AdapterError {
            code: E_DISCORD_ID,
            message: format!("'{id}' is not a Discord id"),
        }),
    }
}

pub fn intents() -> GatewayIntents {
    GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
}

/// The bot token. Its `Debug` never shows the value.
struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

fn read_token(path: &Path) -> Result<Token, AdapterError> {
    let text = std::fs::read_to_string(path).map_err(|err| AdapterError {
        code: E_DISCORD_TOKEN,
        message: format!(
            "cannot read the token file {}: {}",
            path.display(),
            err.kind()
        ),
    })?;
    let token = text.trim();
    if token.is_empty() {
        return Err(AdapterError {
            code: E_DISCORD_TOKEN,
            message: format!("the token file {} is empty", path.display()),
        });
    }
    Ok(Token(token.to_owned()))
}

struct Handler {
    scope: Scope,
    tx: mpsc::Sender<InboundEvent>,
    /// Bumped on every ready and resume, so the core re-sends failed replies.
    reconnects: Arc<watch::Sender<u64>>,
    /// Channel locations already looked up; a channel never changes type or parent.
    locations: Mutex<HashMap<u64, Location>>,
}

impl Handler {
    async fn locate(&self, ctx: &Context, msg: &Message) -> Option<Location> {
        if msg.guild_id.is_none() {
            return Some(Location::Dm);
        }
        let channel_id = msg.channel_id.get();
        if channel_id == self.scope.channel_id {
            return Some(Location::GuildChannel);
        }
        if let Some(location) = self
            .locations
            .lock()
            .expect("locations lock")
            .get(&channel_id)
        {
            return Some(*location);
        }
        let location = match msg.channel(ctx).await {
            Ok(Channel::Guild(channel)) => guild_location(
                matches!(
                    channel.kind,
                    ChannelType::PublicThread
                        | ChannelType::PrivateThread
                        | ChannelType::NewsThread
                ),
                channel.parent_id.map(ChannelId::get),
            ),
            Ok(_) => Location::GuildChannel,
            Err(err) => {
                tracing::warn!(code = E_DISCORD_LOOKUP, channel_id, error = %err, "channel lookup failed, message skipped");
                return None;
            }
        };
        self.locations
            .lock()
            .expect("locations lock")
            .insert(channel_id, location);
        Some(location)
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        let from_self = msg.author.id == ctx.cache.current_user().id;
        if from_self {
            return;
        }
        let Some(location) = self.locate(&ctx, &msg).await else {
            return;
        };
        let attachments: Vec<AttachmentMeta> = msg
            .attachments
            .iter()
            .map(|a| AttachmentMeta {
                filename: a.filename.clone(),
                size: u64::from(a.size),
                content_type: a.content_type.clone(),
                url: AttachmentRef(a.url.clone()),
            })
            .collect();
        let raw = RawMessage {
            message_id: msg.id.get(),
            author_id: msg.author.id.get(),
            author_is_bot: msg.author.bot,
            from_self,
            is_regular: matches!(msg.kind, MessageType::Regular | MessageType::InlineReply),
            channel_id: msg.channel_id.get(),
            location,
            text: &msg.content,
            attachments: &attachments,
        };
        if let Some(event) = map_message(&raw, &self.scope)
            && self.tx.send(event).await.is_err()
        {
            tracing::debug!("inbound receiver closed, discord message dropped");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!(bot_id = ready.user.id.get(), "discord gateway connected");
        self.reconnects.send_modify(|count| *count += 1);
    }

    async fn resume(&self, _ctx: Context, _event: ResumedEvent) {
        tracing::info!("discord gateway resumed");
        self.reconnects.send_modify(|count| *count += 1);
    }
}

#[derive(Default)]
struct Runtime {
    started: bool,
    shard_manager: Option<Arc<ShardManager>>,
    task: Option<JoinHandle<()>>,
}

pub struct DiscordAdapter {
    token: Arc<Token>,
    http: Arc<Http>,
    /// Downloads only; uploads go through `http`.
    file_client: reqwest::Client,
    scope: Scope,
    runtime: Arc<Mutex<Runtime>>,
    shutdown: watch::Sender<bool>,
    reconnects: Arc<watch::Sender<u64>>,
}

impl DiscordAdapter {
    /// Reads the token file once; the token goes only to serenity.
    pub fn new(token_file: &Path, scope: Scope) -> Result<Self, AdapterError> {
        let token = read_token(token_file)?;
        let http = Arc::new(Http::new(&token.0));
        Ok(Self {
            token: Arc::new(token),
            http,
            file_client: files::file_client(),
            scope,
            runtime: Arc::new(Mutex::new(Runtime::default())),
            shutdown: watch::channel(false).0,
            reconnects: Arc::new(watch::channel(0).0),
        })
    }

    pub fn from_config(config: &Config) -> Result<Self, AdapterError> {
        let discord = config.discord.as_ref().ok_or_else(|| AdapterError {
            code: E_MESSENGER_MISSING,
            message: "the [discord] config section is missing".into(),
        })?;
        Self::new(
            &discord.token_file,
            Scope {
                owner_id: discord.owner_id,
                channel_id: discord.channel_id,
            },
        )
    }
}

/// Keeps a gateway connection up until shutdown. A failed or dropped connection (also a
/// rejected token) is retried with backoff instead of ending the process.
async fn gateway_loop(
    token: Arc<Token>,
    handler: Arc<Handler>,
    runtime: Arc<Mutex<Runtime>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut delay = BACKOFF_START;
    loop {
        match Client::builder(&token.0, intents())
            .event_handler_arc(handler.clone())
            .await
        {
            Ok(mut client) => {
                {
                    let mut runtime = runtime.lock().expect("runtime lock");
                    if *shutdown.borrow() {
                        return;
                    }
                    runtime.shard_manager = Some(client.shard_manager.clone());
                }
                let started = Instant::now();
                let result = client.start().await;
                if *shutdown.borrow() {
                    return;
                }
                if let Err(err) = result {
                    tracing::warn!(code = E_DISCORD_GATEWAY, error = %err, "discord gateway stopped");
                }
                if started.elapsed() >= BACKOFF_RESET_AFTER {
                    delay = BACKOFF_START;
                }
            }
            Err(err) => {
                tracing::warn!(code = E_DISCORD_GATEWAY, error = %err, "discord client could not be built");
            }
        }
        tracing::info!(retry_in_secs = delay.as_secs(), "reconnecting to discord");
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = shutdown.wait_for(|stop| *stop) => return,
        }
        delay = next_backoff(delay, BACKOFF_START, BACKOFF_MAX);
    }
}

#[async_trait]
impl MessengerAdapter for DiscordAdapter {
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
        let mut runtime = self.runtime.lock().expect("runtime lock");
        if runtime.started {
            return Err(AdapterError {
                code: E_ADAPTER_START,
                message: "the discord adapter was already started".to_owned(),
            });
        }
        runtime.started = true;
        let (tx, rx) = mpsc::channel(INBOUND_BUFFER);
        let handler = Arc::new(Handler {
            scope: self.scope,
            tx,
            reconnects: self.reconnects.clone(),
            locations: Mutex::new(HashMap::new()),
        });
        runtime.task = Some(tokio::spawn(gateway_loop(
            self.token.clone(),
            handler,
            self.runtime.clone(),
            self.shutdown.subscribe(),
        )));
        Ok(rx)
    }

    async fn send(&self, channel: &str, text: &str) -> Result<SendResult, AdapterError> {
        let channel_id = ChannelId::new(parse_id(channel)?);
        let mut last_id = String::new();
        for chunk in reply_chunks(text) {
            let message = channel_id
                .say(&self.http, chunk)
                .await
                .map_err(|err| AdapterError {
                    code: E_ADAPTER_SEND,
                    message: err.to_string(),
                })?;
            last_id = message.id.get().to_string();
        }
        Ok(SendResult {
            message_id: last_id,
        })
    }

    async fn create_thread(
        &self,
        channel_id: &str,
        from_message_id: &str,
        name: &str,
    ) -> Result<String, AdapterError> {
        let channel_id = ChannelId::new(parse_id(channel_id)?);
        let message_id = MessageId::new(parse_id(from_message_id)?);
        let thread = channel_id
            .create_thread_from_message(
                &self.http,
                message_id,
                CreateThread::new(thread_name(name)),
            )
            .await
            .map_err(|err| AdapterError {
                code: E_ADAPTER_THREAD,
                message: err.to_string(),
            })?;
        Ok(thread.id.get().to_string())
    }

    async fn start_typing(&self, channel: &str) -> Result<TypingHandle, AdapterError> {
        let typing = ChannelId::new(parse_id(channel)?).start_typing(&self.http);
        Ok(TypingHandle::new(move || {
            typing.stop();
        }))
    }

    async fn fetch_attachment(
        &self,
        attachment: &InboundAttachment,
        dest: &Path,
        progress: &Progress,
    ) -> Result<u64, AdapterError> {
        let url = files::check_url(&attachment.reference.0, &DOWNLOAD_HOSTS)?;
        files::fetch_to_file(&self.file_client, url, None, dest, false, progress).await
    }

    async fn send_file(
        &self,
        target: &str,
        path: &Path,
        name: &str,
    ) -> Result<SendResult, AdapterError> {
        let message_id = upload_file(&self.http, parse_id(target)?, path, name).await?;
        Ok(SendResult { message_id })
    }

    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        Some(self.reconnects.subscribe())
    }

    async fn shutdown(&self) {
        self.shutdown.send_replace(true);
        let (shard_manager, task) = {
            let mut runtime = self.runtime.lock().expect("runtime lock");
            (runtime.shard_manager.take(), runtime.task.take())
        };
        if let Some(shard_manager) = shard_manager {
            shard_manager.shutdown_all().await;
        }
        if let Some(task) = task
            && let Err(err) = task.await
        {
            tracing::warn!(error = %err, "discord gateway task ended abnormally");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const OWNER: u64 = 111;
    const OTHER: u64 = 333;
    const CHANNEL: u64 = 222;
    const THREAD: u64 = 444;

    fn scope() -> Scope {
        Scope {
            owner_id: OWNER,
            channel_id: CHANNEL,
        }
    }

    fn raw(location: Location, channel_id: u64, author_id: u64, text: &str) -> RawMessage<'_> {
        RawMessage {
            message_id: 1_300_000_000_000_000_001,
            author_id,
            author_is_bot: false,
            from_self: false,
            is_regular: true,
            channel_id,
            location,
            text,
            attachments: &[],
        }
    }

    fn meta(name: &str, url: &str) -> AttachmentMeta {
        AttachmentMeta {
            filename: name.to_owned(),
            size: 1234,
            content_type: Some("application/pdf".to_owned()),
            url: AttachmentRef(url.to_owned()),
        }
    }

    const CDN_URL: &str = "https://cdn.discordapp.com/attachments/222/555/report.pdf";

    #[test]
    fn attachment_only_message_maps() {
        let metas = [meta("report.pdf", CDN_URL)];
        let mut message = raw(Location::GuildChannel, CHANNEL, OWNER, "");
        message.attachments = &metas;
        let event = map_message(&message, &scope()).expect("event");
        assert!(event.text.is_empty());
        assert_eq!(event.attachments.len(), 1);
    }

    #[test]
    fn attachment_metadata_copied() {
        let metas = [
            meta("report.pdf", CDN_URL),
            AttachmentMeta {
                filename: "photo.png".to_owned(),
                size: 7,
                content_type: None,
                url: AttachmentRef("https://media.discordapp.net/x/photo.png".to_owned()),
            },
        ];
        let location = Location::Thread {
            parent_id: Some(CHANNEL),
        };
        let mut message = raw(location, THREAD, OWNER, "look");
        message.attachments = &metas;
        let event = map_message(&message, &scope()).expect("event");
        assert_eq!(event.text, "look");
        assert_eq!(
            event.attachments,
            vec![
                InboundAttachment {
                    name: "report.pdf".into(),
                    size: 1234,
                    mime: Some("application/pdf".into()),
                    reference: AttachmentRef(CDN_URL.into()),
                },
                InboundAttachment {
                    name: "photo.png".into(),
                    size: 7,
                    mime: None,
                    reference: AttachmentRef("https://media.discordapp.net/x/photo.png".into()),
                },
            ]
        );
        assert!(!format!("{event:?}").contains("cdn.discordapp.com"));
    }

    #[test]
    fn non_owner_attachment_still_dropped() {
        let metas = [meta("report.pdf", CDN_URL)];
        for (location, channel_id, author) in [
            (Location::Dm, 555, OTHER),
            (Location::GuildChannel, 999, OWNER),
            (
                Location::Thread {
                    parent_id: Some(999),
                },
                THREAD,
                OWNER,
            ),
        ] {
            let mut message = raw(location, channel_id, author, "");
            message.attachments = &metas;
            assert_eq!(map_message(&message, &scope()), None, "{location:?}");
        }
        let mut own = raw(Location::GuildChannel, CHANNEL, OWNER, "");
        own.attachments = &metas;
        own.from_self = true;
        assert_eq!(map_message(&own, &scope()), None);
    }

    #[tokio::test]
    async fn download_host_allowlist() {
        assert!(files::check_url(CDN_URL, &DOWNLOAD_HOSTS).is_ok());
        assert!(files::check_url("https://media.discordapp.net/a/b.png", &DOWNLOAD_HOSTS).is_ok());
        for bad in [
            "https://cdn.discordapp.com.evil.test/attachments/1/2/x",
            "http://cdn.discordapp.com/attachments/1/2/x",
            "https://evil.test/x",
            "",
        ] {
            assert_eq!(
                files::check_url(bad, &DOWNLOAD_HOSTS).unwrap_err().code,
                files::E_FILE_HOST,
                "{bad:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let adapter =
            DiscordAdapter::new(&token_file(dir.path(), "test-token-111"), scope()).unwrap();
        let dest = dir.path().join("x");
        let attachment = InboundAttachment {
            name: "x".into(),
            size: 1,
            mime: None,
            reference: AttachmentRef("https://cdn.discordapp.com.evil.test/x".into()),
        };
        let err = adapter
            .fetch_attachment(&attachment, &dest, &Progress::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, files::E_FILE_HOST);
        assert!(!err.message.contains("evil.test"));
        assert!(!dest.exists());
    }

    #[test]
    fn default_hints_mention_send_file() {
        assert!(DEFAULT_HINTS.contains("send_file"));
        assert!(DEFAULT_HINTS.contains("at most 10 files"));
    }

    #[test]
    fn upload_error_classification() {
        for status in [400, 403, 404, 413] {
            assert_eq!(
                upload_error_code(Some(status)),
                E_ADAPTER_UPLOAD_DENIED,
                "{status}"
            );
        }
        for status in [Some(429), Some(500), Some(502), None] {
            assert_eq!(upload_error_code(status), E_ADAPTER_UPLOAD, "{status:?}");
        }
    }

    #[tokio::test]
    async fn unreadable_local_file_is_upload_denied() {
        let dir = tempfile::tempdir().unwrap();
        let adapter =
            DiscordAdapter::new(&token_file(dir.path(), "test-token-111"), scope()).unwrap();
        let locked = dir.path().join("locked.txt");
        std::fs::write(&locked, b"secret").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        for path in [locked.clone(), dir.path().join("missing.txt")] {
            let err = adapter
                .send_file("222", &path, "locked.txt")
                .await
                .unwrap_err();
            assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED, "{path:?}");
            assert!(!err.message.contains("locked.txt"));
        }
    }

    /// One request as the loopback server saw it.
    struct Seen {
        head: String,
        body: Vec<u8>,
    }

    #[derive(Clone, Copy)]
    enum Reply {
        Status(u16, &'static str),
        DropAfterHead,
    }

    const MESSAGE_JSON: &str = r#"{"id":"333","channel_id":"222","author":{"id":"111","username":"fake","discriminator":"0","avatar":null},"content":"","timestamp":"2026-01-01T00:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0}"#;
    const WAIT: Duration = Duration::from_secs(30);

    /// A loopback HTTP server: every request on every connection is sent to the test and
    /// answered with `reply`.
    async fn discord_stub(reply: Reply) -> (String, mpsc::UnboundedReceiver<Seen>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        let head_end = loop {
                            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break i + 4;
                            }
                            let mut chunk = [0u8; 8192];
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                        let length = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if let Reply::DropAfterHead = reply {
                            let _ = tx.send(Seen {
                                head,
                                body: Vec::new(),
                            });
                            return;
                        }
                        while buf.len() < head_end + length {
                            let mut chunk = [0u8; 65536];
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let body = buf[head_end..head_end + length].to_vec();
                        buf.drain(..head_end + length);
                        let _ = tx.send(Seen { head, body });
                        let Reply::Status(status, json) = reply else {
                            return;
                        };
                        let response = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{json}",
                            json.len()
                        );
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (base, rx)
    }

    fn loopback_http(base: &str) -> Http {
        serenity::all::HttpBuilder::new("test-token-111")
            .proxy(base)
            .ratelimiter_disabled(true)
            .build()
    }

    fn temp_file(dir: &Path, name: &str, data: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    async fn next_request(rx: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("request in time")
            .expect("server running")
    }

    #[tokio::test]
    async fn upload_sends_file_once_through_serenity() {
        let (base, mut rx) = discord_stub(Reply::Status(200, MESSAGE_JSON)).await;
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        let path = temp_file(dir.path(), "report.txt", &data);
        let result = tokio::time::timeout(
            WAIT,
            upload_file(&loopback_http(&base), CHANNEL, &path, "report.txt"),
        )
        .await
        .expect("upload in time");
        assert_eq!(result, Ok("333".to_owned()));
        let seen = next_request(&mut rx).await;
        assert!(rx.try_recv().is_err(), "exactly one request");
        assert!(
            seen.head
                .starts_with("POST /api/v10/channels/222/messages "),
            "{}",
            seen.head.lines().next().unwrap_or_default()
        );
        assert!(
            seen.head
                .lines()
                .any(|l| l.eq_ignore_ascii_case("authorization: Bot test-token-111"))
        );
        let marker = b"filename=\"report.txt\"";
        let at = seen
            .body
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("file part");
        let start = at
            + seen.body[at..]
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("part head")
            + 4;
        assert_eq!(&seen.body[start..start + data.len()], &data[..]);
    }

    #[tokio::test]
    async fn upload_size_refusal_is_denied() {
        let (base, mut rx) = discord_stub(Reply::Status(
            413,
            r#"{"code":40005,"message":"Request entity too large"}"#,
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(dir.path(), "big.bin", b"abc");
        let err = tokio::time::timeout(
            WAIT,
            upload_file(&loopback_http(&base), CHANNEL, &path, "big.bin"),
        )
        .await
        .expect("upload in time")
        .unwrap_err();
        assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);
        next_request(&mut rx).await;
        assert!(rx.try_recv().is_err(), "exactly one request");
    }

    #[tokio::test]
    async fn upload_server_error_is_transient() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(dir.path(), "a.txt", b"abc");
        for reply in [
            Reply::Status(500, r#"{"code":0,"message":"Internal Server Error"}"#),
            Reply::DropAfterHead,
        ] {
            let (base, mut rx) = discord_stub(reply).await;
            let err = tokio::time::timeout(
                WAIT,
                upload_file(&loopback_http(&base), CHANNEL, &path, "a.txt"),
            )
            .await
            .expect("upload in time")
            .unwrap_err();
            assert_eq!(err.code, E_ADAPTER_UPLOAD);
            next_request(&mut rx).await;
        }
    }

    #[tokio::test]
    async fn upload_never_logs_file_bytes_name_or_token() {
        use tracing_subscriber::fmt::format::FmtSpan;
        const MARKER: &str = "S3LOGMARK-file-bytes-7q2w";
        const NAME: &str = "s3-secret-name-5k8.txt";
        let capture = crate::logging::Capture::default();
        let _guard = tracing::subscriber::set_default(crate::logging::subscriber(
            Some("trace"),
            FmtSpan::NEW | FmtSpan::CLOSE,
            capture.clone(),
        ));
        let (base, mut rx) = discord_stub(Reply::Status(200, MESSAGE_JSON)).await;
        let dir = tempfile::tempdir().unwrap();
        let data = MARKER.repeat(200 * 1024 / MARKER.len() + 1).into_bytes();
        let path = temp_file(dir.path(), NAME, &data);
        let result = tokio::time::timeout(
            WAIT,
            upload_file(&loopback_http(&base), CHANNEL, &path, NAME),
        )
        .await
        .expect("upload in time");
        assert_eq!(result, Ok("333".to_owned()));
        next_request(&mut rx).await;
        tracing::info!(target: "moraebot::discord", "upload-log-probe-8d2");
        let text = capture.contents();
        assert!(text.contains("upload-log-probe-8d2"), "capture is wired");
        for leak in [
            MARKER,
            "83, 51, 76, 79, 71, 77, 65, 82",
            NAME,
            "test-token-111",
        ] {
            assert!(!text.contains(leak), "leaked {leak}");
        }
    }

    #[test]
    fn owner_message_in_configured_channel_maps_to_event() {
        let event = map_message(
            &raw(Location::GuildChannel, CHANNEL, OWNER, "hello"),
            &scope(),
        )
        .expect("event");
        assert_eq!(
            event,
            InboundEvent {
                message_id: "1300000000000000001".into(),
                author_id: "111".into(),
                author_is_bot: false,
                channel_id: "222".into(),
                thread_id: None,
                is_dm: false,
                text: "hello".into(),
                attachments: Vec::new(),
            }
        );
    }

    #[test]
    fn other_authors_in_scope_still_reach_the_core_access_check() {
        let event = map_message(&raw(Location::GuildChannel, CHANNEL, OTHER, "hi"), &scope())
            .expect("event");
        assert_eq!(event.author_id, "333");
    }

    #[test]
    fn bot_flag_is_carried() {
        let mut message = raw(Location::GuildChannel, CHANNEL, OWNER, "hi");
        message.author_is_bot = true;
        assert!(
            map_message(&message, &scope())
                .expect("event")
                .author_is_bot
        );
    }

    #[test]
    fn own_and_system_messages_are_dropped() {
        let mut own = raw(Location::GuildChannel, CHANNEL, OWNER, "hi");
        own.from_self = true;
        assert_eq!(map_message(&own, &scope()), None);
        let mut system = raw(Location::GuildChannel, CHANNEL, OWNER, "hi");
        system.is_regular = false;
        assert_eq!(map_message(&system, &scope()), None);
    }

    #[test]
    fn other_guild_channels_are_dropped() {
        assert_eq!(
            map_message(&raw(Location::GuildChannel, 999, OWNER, "hi"), &scope()),
            None
        );
    }

    #[test]
    fn thread_under_configured_channel_maps_with_thread_id_and_parent() {
        let location = Location::Thread {
            parent_id: Some(CHANNEL),
        };
        let event = map_message(&raw(location, THREAD, OWNER, "more"), &scope()).expect("event");
        assert_eq!(event.channel_id, "222");
        assert_eq!(event.thread_id.as_deref(), Some("444"));
        assert!(!event.is_dm);
    }

    #[test]
    fn threads_under_other_or_unknown_parents_are_dropped() {
        for parent_id in [Some(999), None] {
            let location = Location::Thread { parent_id };
            assert_eq!(
                map_message(&raw(location, THREAD, OWNER, "x"), &scope()),
                None
            );
        }
    }

    #[test]
    fn owner_dm_maps_to_dm_event_and_other_dms_are_dropped() {
        let event = map_message(&raw(Location::Dm, 555, OWNER, "dm"), &scope()).expect("event");
        assert!(event.is_dm);
        assert_eq!(event.channel_id, "555");
        assert_eq!(event.thread_id, None);
        assert_eq!(
            map_message(&raw(Location::Dm, 555, OTHER, "dm"), &scope()),
            None
        );
    }

    #[test]
    fn empty_content_maps_to_event_with_empty_text() {
        let event =
            map_message(&raw(Location::GuildChannel, CHANNEL, OWNER, ""), &scope()).expect("event");
        assert!(event.text.is_empty());
    }

    #[test]
    fn guild_location_distinguishes_threads() {
        assert_eq!(guild_location(false, Some(1)), Location::GuildChannel);
        assert_eq!(
            guild_location(true, Some(CHANNEL)),
            Location::Thread {
                parent_id: Some(CHANNEL)
            }
        );
    }

    #[test]
    fn thread_name_truncates_korean_by_characters() {
        let text = "가".repeat(80);
        let name = thread_name(&text);
        assert_eq!(name.chars().count(), THREAD_NAME_MAX_CHARS);
        assert_eq!(name, "가".repeat(THREAD_NAME_MAX_CHARS));
    }

    #[test]
    fn thread_name_collapses_whitespace() {
        assert_eq!(
            thread_name("  fix\nthe   build \t now "),
            "fix the build now"
        );
    }

    #[test]
    fn thread_name_falls_back_below_two_characters() {
        for short in ["", "   ", "a", "\n가\n"] {
            assert_eq!(thread_name(short), THREAD_NAME_FALLBACK, "{short:?}");
        }
        assert_eq!(thread_name("ab"), "ab");
        assert_eq!(thread_name("가나"), "가나");
    }

    #[test]
    fn thread_name_never_ends_with_a_space_after_truncation() {
        let text = format!("{} tail", "x".repeat(THREAD_NAME_MAX_CHARS - 1));
        assert_eq!(thread_name(&text), "x".repeat(THREAD_NAME_MAX_CHARS - 1));
    }

    #[test]
    fn reply_chunks_replace_blank_reply_with_english_placeholder() {
        assert_eq!(reply_chunks(""), vec![EMPTY_REPLY.to_owned()]);
        assert_eq!(reply_chunks(" \n "), vec![EMPTY_REPLY.to_owned()]);
    }

    #[test]
    fn reply_chunks_stay_within_limit_and_skip_blank_chunks() {
        let text = format!("{}\n{}", "가".repeat(4500), "\n".repeat(2500));
        let chunks = reply_chunks(&text);
        assert!(chunks.iter().all(|c| c.chars().count() <= MESSAGE_LIMIT));
        assert!(chunks.iter().all(|c| !c.trim().is_empty()));
        assert_eq!(chunks.concat().chars().filter(|c| *c == '가').count(), 4500);
    }

    #[test]
    fn parse_id_rejects_zero_and_non_numbers() {
        assert_eq!(parse_id("222").unwrap(), 222);
        for bad in ["0", "", "abc", "-1", "thread-1"] {
            assert_eq!(parse_id(bad).unwrap_err().code, E_DISCORD_ID, "{bad:?}");
        }
    }

    #[test]
    fn intents_include_message_content() {
        let wanted = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;
        assert_eq!(intents(), wanted);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let next = |delay| next_backoff(delay, BACKOFF_START, BACKOFF_MAX);
        assert_eq!(next(BACKOFF_START), Duration::from_secs(10));
        assert_eq!(next(Duration::from_secs(200)), BACKOFF_MAX);
        assert_eq!(next(BACKOFF_MAX), BACKOFF_MAX);
    }

    fn token_file(dir: &Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("discord.token");
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn missing_or_empty_token_file_is_refused_with_code() {
        let dir = tempfile::tempdir().unwrap();
        let missing = DiscordAdapter::new(&dir.path().join("nope"), scope())
            .err()
            .unwrap();
        assert_eq!(missing.code, E_DISCORD_TOKEN);
        let empty = DiscordAdapter::new(&token_file(dir.path(), " \n"), scope())
            .err()
            .unwrap();
        assert_eq!(empty.code, E_DISCORD_TOKEN);
    }

    #[test]
    fn token_debug_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let token = read_token(&token_file(dir.path(), "TOKEN_SENTINEL_9f3a\n")).unwrap();
        assert_eq!(token.0, "TOKEN_SENTINEL_9f3a");
        assert!(!format!("{token:?}").contains("TOKEN_SENTINEL_9f3a"));
    }

    #[tokio::test]
    async fn invalid_ids_fail_before_any_network_call() {
        let dir = tempfile::tempdir().unwrap();
        let adapter =
            DiscordAdapter::new(&token_file(dir.path(), "TOKEN_SENTINEL_9f3a"), scope()).unwrap();
        assert_eq!(adapter.platform(), PLATFORM);
        assert_eq!(
            adapter.send("thread-1", "hi").await.unwrap_err().code,
            E_DISCORD_ID
        );
        assert_eq!(
            adapter
                .create_thread("222", "0", "name")
                .await
                .unwrap_err()
                .code,
            E_DISCORD_ID
        );
        assert_eq!(
            adapter.start_typing("").await.err().unwrap().code,
            E_DISCORD_ID
        );
    }
}
