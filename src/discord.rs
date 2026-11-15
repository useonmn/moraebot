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
    Channel, ChannelId, ChannelType, Client, Context, CreateThread, EventHandler, GatewayIntents,
    Http, Message, MessageId, MessageType, Ready, ResumedEvent, ShardManager,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::adapter::{
    AdapterError, E_ADAPTER_SEND, E_ADAPTER_START, E_ADAPTER_THREAD, InboundEvent,
    MessengerAdapter, SendResult, TypingHandle, split_reply,
};
use crate::config::Config;

pub const PLATFORM: &str = "discord";
/// Discord's message length limit, counted in characters.
pub const MESSAGE_LIMIT: usize = 2000;
pub const THREAD_NAME_MAX_CHARS: usize = 50;
pub const THREAD_NAME_MIN_CHARS: usize = 2;
pub const THREAD_NAME_FALLBACK: &str = "moraebot";
/// Sent instead of an empty reply, which Discord would reject.
pub const EMPTY_REPLY: &str = "(empty reply)";

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
}

/// Turns a message into a core event, or `None` when it is outside the listening scope.
/// Empty text is kept as an empty `text` so the core can answer it with its fixed notice.
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
    })
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

pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_MAX)
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
        message: format!("cannot read the token file {}: {}", path.display(), err.kind()),
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
        if let Some(location) = self.locations.lock().expect("locations lock").get(&channel_id) {
            return Some(*location);
        }
        let location = match msg.channel(ctx).await {
            Ok(Channel::Guild(channel)) => guild_location(
                matches!(
                    channel.kind,
                    ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
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
        let raw = RawMessage {
            message_id: msg.id.get(),
            author_id: msg.author.id.get(),
            author_is_bot: msg.author.bot,
            from_self,
            is_regular: matches!(msg.kind, MessageType::Regular | MessageType::InlineReply),
            channel_id: msg.channel_id.get(),
            location,
            text: &msg.content,
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
            scope,
            runtime: Arc::new(Mutex::new(Runtime::default())),
            shutdown: watch::channel(false).0,
            reconnects: Arc::new(watch::channel(0).0),
        })
    }

    pub fn from_config(config: &Config) -> Result<Self, AdapterError> {
        Self::new(
            &config.token_file,
            Scope {
                owner_id: config.owner_id,
                channel_id: config.discord.channel_id,
            },
        )
    }

    pub async fn shutdown(&self) {
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
        delay = next_backoff(delay);
    }
}

#[async_trait]
impl MessengerAdapter for DiscordAdapter {
    fn platform(&self) -> &'static str {
        PLATFORM
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
            .create_thread_from_message(&self.http, message_id, CreateThread::new(thread_name(name)))
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

    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        Some(self.reconnects.subscribe())
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
        }
    }

    #[test]
    fn owner_message_in_configured_channel_maps_to_event() {
        let event = map_message(&raw(Location::GuildChannel, CHANNEL, OWNER, "hello"), &scope())
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
        assert!(map_message(&message, &scope()).expect("event").author_is_bot);
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
            assert_eq!(map_message(&raw(location, THREAD, OWNER, "x"), &scope()), None);
        }
    }

    #[test]
    fn owner_dm_maps_to_dm_event_and_other_dms_are_dropped() {
        let event = map_message(&raw(Location::Dm, 555, OWNER, "dm"), &scope()).expect("event");
        assert!(event.is_dm);
        assert_eq!(event.channel_id, "555");
        assert_eq!(event.thread_id, None);
        assert_eq!(map_message(&raw(Location::Dm, 555, OTHER, "dm"), &scope()), None);
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
        assert_eq!(thread_name("  fix\nthe   build \t now "), "fix the build now");
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
        assert_eq!(next_backoff(BACKOFF_START), Duration::from_secs(10));
        assert_eq!(next_backoff(Duration::from_secs(200)), BACKOFF_MAX);
        assert_eq!(next_backoff(BACKOFF_MAX), BACKOFF_MAX);
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
        let missing = DiscordAdapter::new(&dir.path().join("nope"), scope()).err().unwrap();
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
        assert_eq!(adapter.send("thread-1", "hi").await.unwrap_err().code, E_DISCORD_ID);
        assert_eq!(
            adapter.create_thread("222", "0", "name").await.unwrap_err().code,
            E_DISCORD_ID
        );
        assert_eq!(adapter.start_typing("").await.err().unwrap().code, E_DISCORD_ID);
    }
}
