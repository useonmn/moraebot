//! Core dispatcher: inbound event -> access check -> store -> one global worker -> omp -> reply.
//!
//! Order of an inbound message: drop authors that are neither the owner nor listed in the
//! active messenger's `allow` list (nothing stored); drop a listed bot's message once its
//! conversation reached `max_bot_replies` in the window (no limit when unset); answer a
//! listed `chat` sender with one fixed notice ([`N_CHAT_UNAVAILABLE`]; never stored, no
//! file fetched, never forwarded to the lead); answer `/` messages and empty messages without attachments with one fixed notice (never
//! stored, never forwarded), refuse more than [`files::MAX_FILES`] attachments (nothing
//! stored, nothing fetched), pick the conversation key, store the row FIRST (a duplicate
//! stops here), and only then open a thread for a message posted directly in the channel.
//!
//! Attachments are forwarded: they are fetched only after the row is stored and its thread
//! opened, outside the admission lock, into `<work_dir>/.moraebot-inbox/<key>/<row>/`, each
//! under the stall watchdog. The row stays out of the FIFO until every file is saved; one
//! failed file fails the row with one notice. The saved paths are listed in the prompt.
//!
//! A row that starts its own thread (key `<platform>:th:<its own message id>`) is run only
//! once that thread is known (`known_threads`). A row left without a thread by a crash
//! between admission and thread creation gets one more creation attempt at startup (or
//! at dequeue); if that fails it becomes `failed` with one notice in the channel, and it
//! never reaches omp.
//!
//! One lead omp session serves every conversation. Each owner row reaches it as
//! [`lead::owner_input`] (a tag line naming its conversation and row, then the text with
//! the saved-files block); a lane-result row (`origin = 'lane'`) carries its stored text
//! and skips the owner check. Every reply goes to `reply_target(row.conversation_key)`.
//!
//! A message of the conversation whose lead turn is running is steered into that turn
//! once its files are saved, only when it comes from the same sender as the turn, when the store accepts the claim ([`Store::steer_into`]);
//! otherwise it is queued. A turn and its steered row form one group: one reply on the
//! main row, or one outcome for both (`needs_check` wins over `failed`).
//!
//! One worker serves the global FIFO with one lead turn in flight. A reply is stored
//! (`reply_pending`) before it is sent and the row becomes `done` only after every chunk
//! was accepted; a failed send is retried after the adapter reconnects and at startup.
//! Interrupted work (`needs_check`) is never re-prompted; its conversation gets one
//! notice, also across restarts (`notice_sent`), and its older queued rows are held: they
//! are marked `needs_check` / [`E_DISPATCH_HELD`] in the same store transaction as the
//! interruption, so the hold survives a restart.
//!
//! Files the agent queued with `send_file` are stored with the reply and uploaded after
//! its text, each marked sent or skipped on its own, so a resend sends only what is
//! missing. A file the messenger refuses, that cannot be read, that ran past
//! [`files::UPLOAD_TIMEOUT`] or that reached [`files::MAX_UPLOAD_ATTEMPTS`] is skipped;
//! the owner gets one notice per reply.
//!
//! At startup, an onboarding request left by `moraebot init` or `moraebot onboarding` starts
//! the first meeting and posts one fixed greeting: in the owner's DM (in the configured
//! channel when the DM fails), or in the channel when the request says so.
//!
//! Lane news: after every lane watchdog tick, the newest unposted progress note of each
//! running lane is delivered in every conversation that had no owner message, delivered
//! reply or delivered news for [`NEWS_AFTER`]; the lead is not involved. Each lane has one
//! news message: the first note is posted, later notes edit it, and a failed edit posts a
//! new one. A note the messenger did not accept is tried again on the next tick.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc, watch};

use crate::access::{BotReplies, Decision, DropReason, E_ACCESS_NOT_OWNER, Policy, Sender};
use crate::adapter::{
    AdapterError, ConversationKey, E_ADAPTER_UPLOAD_DENIED, InboundAttachment, InboundEvent,
    MessengerAdapter, split_reply,
};
use crate::config::Config;
use crate::files::{self, OutFile, SavedFile};
use crate::lane::{LaneManager, LaneNews};
use crate::lead;
use crate::omp::{E_COST_CAP, GroupReport, OmpManager, Outcome, key_slug};
use crate::onboarding::{self, E_ONBOARDING_REQUEST, GreetTarget};
pub use crate::store::E_DISPATCH_HELD;
use crate::store::{Admitted, InboxRow, State, Store, StoreError};

/// Empty message without attachments, or `/` message: not forwarded to omp.
pub const N_UNSUPPORTED_INPUT: &str = "N_UNSUPPORTED_INPUT";
/// A thread could not be opened for a channel message; the row is `failed`.
pub const N_THREAD_FAILED: &str = "N_THREAD_FAILED";
/// The turn failed; the row is `failed`.
pub const N_FAILED: &str = "N_FAILED";
/// The configured cost limit stopped the turn.
pub const N_COST_CAP: &str = "N_COST_CAP";
/// The turn was interrupted (lost child, timeout, held session); the row is `needs_check`.
pub const N_INTERRUPTED: &str = "N_INTERRUPTED";
/// omp handled the message without running the agent.
pub const N_NO_AGENT: &str = "N_NO_AGENT";
/// Appended to a reply that was cut at [`MAX_REPLY_MESSAGES`].
pub const N_REPLY_TRUNCATED: &str = "N_REPLY_TRUNCATED";
/// Too many attachments, or one could not be fetched or saved; the message was not run.
pub const N_FILE_NOT_RECEIVED: &str = "N_FILE_NOT_RECEIVED";
/// The messenger refused the download (missing permission); the message was not run.
pub const N_FILE_NOT_ALLOWED: &str = "N_FILE_NOT_ALLOWED";
/// A file the agent queued was skipped or its upload failed; posted once per reply.
pub const N_FILE_NOT_SENT: &str = "N_FILE_NOT_SENT";
/// A lane was running when the gateway stopped; it was not resumed.
pub const N_LANE_INTERRUPTED: &str = "N_LANE_INTERRUPTED";
/// The one greeting after an onboarding request.
pub const N_ONBOARDING_HELLO: &str = "N_ONBOARDING_HELLO";
/// A listed `chat` sender's message: chat replies are not available yet; not stored.
pub const N_CHAT_UNAVAILABLE: &str = "N_CHAT_UNAVAILABLE";

/// Stored row error: the conversation key has no reply target.
pub const E_DISPATCH_KEY: &str = "E_DISPATCH_KEY";

/// Longer replies are cut with a note.
pub const MAX_REPLY_MESSAGES: usize = 10;

/// A conversation must be this quiet before lane news is posted in it.
pub const NEWS_AFTER: Duration = Duration::from_secs(20 * 60);

/// The fixed English text of a notice code.
pub fn notice_text(code: &str) -> &'static str {
    match code {
        N_UNSUPPORTED_INPUT => {
            "Messages starting with \"/\" and empty messages are not sent to the agent."
        }
        N_THREAD_FAILED => {
            "Could not open a thread for this message, so it was not run. Send it again if needed."
        }
        N_FAILED => "The task failed and did not finish.",
        N_COST_CAP => "The cost limit was reached. Continue in a new thread.",
        N_INTERRUPTED => {
            "The task was interrupted. Part of it may already have run. Send it again if needed."
        }
        N_NO_AGENT => "omp handled this message without running the agent.",
        N_REPLY_TRUNCATED => "[Reply cut: it was longer than 10 messages.]",
        N_FILE_NOT_RECEIVED => {
            "A file could not be received, so the message was not run. At most 10 files per \
             message. Send it again if needed."
        }
        N_FILE_NOT_ALLOWED => {
            "moraebot is not allowed to download this file, so the message was not run. Check \
             the bot's file permission (README: Upgrading) and send it again."
        }
        N_FILE_NOT_SENT => {
            "A file from the agent could not be sent, or sending it was interrupted. If this \
             keeps happening, check the bot's file permission (README: Upgrading)."
        }
        N_LANE_INTERRUPTED => {
            "A background task was interrupted by a restart and was not resumed. Ask again if it \
             is still needed."
        }
        N_ONBOARDING_HELLO => {
            "Hi! I'm your moraebot agent, and this is our first chat. When you reply, I'll ask \
             what to call me, how I should talk to you, and a little about you. Reply skip if \
             you'd rather not."
        }
        N_CHAT_UNAVAILABLE => {
            "This account is allowed to chat with moraebot, but chat replies are not available \
             in this version yet, so the message was not run."
        }
        _ => "Something went wrong.",
    }
}

/// The messages to post for a reply: at most [`MAX_REPLY_MESSAGES`] chunks of at most
/// `limit` characters (the adapter's limit), never blank; a longer reply ends with the
/// cut note.
pub fn reply_messages(text: &str, limit: usize) -> Vec<String> {
    let mut chunks: Vec<String> = split_reply(text, limit)
        .into_iter()
        .filter(|chunk| !chunk.trim().is_empty())
        .collect();
    if chunks.len() <= MAX_REPLY_MESSAGES {
        return chunks;
    }
    chunks.truncate(MAX_REPLY_MESSAGES);
    let note = notice_text(N_REPLY_TRUNCATED);
    let room = limit - note.chars().count() - 1;
    let last = chunks.pop().unwrap_or_default();
    let head: String = last.chars().take(room).collect();
    chunks.push(format!("{head}\n{note}"));
    chunks
}

/// The channel a conversation's replies go to: the thread id or the DM channel id.
pub fn reply_target(key: &str) -> Option<&str> {
    key.split_once(":th:")
        .or_else(|| key.split_once(":dm:"))
        .map(|(_, id)| id)
        .filter(|id| !id.is_empty())
}

/// One lane's line in a news message.
pub fn news_line(lane_id: i64, text: &str) -> String {
    format!("[lane {lane_id}] {text}")
}

/// Whether the row opened its own thread: its key is `<platform>:th:<its message id>`.
fn is_thread_starter(row: &InboxRow) -> bool {
    row.conversation_key == ConversationKey::thread(&row.platform, &row.message_id).as_str()
}

/// The notice for a row whose files could not be received.
fn file_notice(code: &str) -> &'static str {
    if code == files::E_FILE_DENIED {
        N_FILE_NOT_ALLOWED
    } else {
        N_FILE_NOT_RECEIVED
    }
}

/// The declared attachment metadata stored at admission: name, size and mime only, never
/// the reference (a signed or token-gated URL).
fn declared_json(attachments: &[InboundAttachment]) -> String {
    let declared: Vec<serde_json::Value> = attachments
        .iter()
        .map(|a| serde_json::json!({"name": a.name, "size": a.size, "mime": a.mime}))
        .collect();
    serde_json::Value::Array(declared).to_string()
}

/// The out-file list stored with a reply for the files `send_file` queued; `None` when
/// there are none.
fn out_files_json(files: Vec<PathBuf>) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let entries: Vec<OutFile> = files
        .into_iter()
        .map(|path| OutFile {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_owned()),
            path,
            sent: false,
            skipped: false,
            attempting: false,
            attempts: 0,
        })
        .collect();
    Some(serde_json::to_string(&entries).expect("out-file list serializes to JSON"))
}

/// The reply text of one group member; `None` when it did not reply.
fn reply_text(outcome: &Outcome) -> Option<&str> {
    match outcome {
        Outcome::Reply(text) => Some(text),
        Outcome::NoAgent => Some(notice_text(N_NO_AGENT)),
        Outcome::Failed { .. } | Outcome::NeedsCheck { .. } => None,
    }
}

/// The group's one reply: the texts in send order, consecutive equal texts once, joined
/// by a blank line.
fn combine_replies<'a>(texts: impl IntoIterator<Item = &'a str>) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for text in texts {
        if kept.last() != Some(&text) {
            kept.push(text);
        }
    }
    kept.join("\n\n")
}

/// Resolves at the adapter's next reconnect; never, when it has no reconnect signal.
async fn next_reconnect(reconnects: &mut Option<watch::Receiver<u64>>) {
    let Some(rx) = reconnects else {
        return std::future::pending().await;
    };
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// What happened to one inbound event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handled {
    Dropped(DropReason),
    /// Answered with this notice code; nothing stored.
    Notice(&'static str),
    Duplicate,
    /// Stored, but the thread could not be opened; the row is `failed`.
    ThreadFailed(i64),
    /// The message's files were refused or not received, so it is not run; carries the
    /// notice code. A count refusal stores nothing; otherwise the row is `failed` (or kept
    /// held under its earlier notice when it was held meanwhile).
    FilesRejected(&'static str),
    Queued(i64),
    /// Stored and sent into the running lead turn of its conversation; answered in that
    /// turn's reply.
    Steered(i64),
}

/// How often a conversation saw an owner message, a delivered reply or posted news, and
/// the tick time that count was last seen to change.
#[derive(Default)]
struct Talk {
    said: u64,
    seen: Option<(u64, Instant)>,
}

pub struct Dispatcher {
    owner_id: String,
    /// Who may talk to moraebot, from the active messenger's config.
    access: Policy,
    /// Replies to listed bots per conversation, capped by `max_bot_replies`.
    bot_replies: Mutex<BotReplies>,
    adapter: Arc<dyn MessengerAdapter>,
    store: Arc<Mutex<Store>>,
    omp: OmpManager,
    /// Absolute work folder: received files are saved under it, sent files must be in it.
    work_dir: PathBuf,
    /// The moraebot home: never a source of sent files.
    home: PathBuf,
    /// The configured channel: the onboarding greeting goes there when asked, or when the
    /// owner's DM fails.
    channel_id: String,
    /// Held while a row is admitted and its thread opened, and while the worker dequeues,
    /// so the worker never picks a row whose thread does not exist yet.
    admission: tokio::sync::Mutex<()>,
    /// Rows whose reply send failed in this process; re-sent by [`Self::resend_pending`].
    pending: Mutex<Vec<i64>>,
    wake: Notify,
    /// Talk per conversation key, for lane news.
    talk: Mutex<HashMap<String, Talk>>,
}

impl Dispatcher {
    pub fn new(
        config: &Config,
        adapter: Arc<dyn MessengerAdapter>,
        store: Arc<Mutex<Store>>,
        omp: OmpManager,
    ) -> Self {
        Self {
            owner_id: config.owner_id(),
            access: Policy::from_config(config),
            bot_replies: Mutex::new(BotReplies::from_config(config)),
            adapter,
            store,
            omp,
            work_dir: std::path::absolute(&config.work_dir)
                .unwrap_or_else(|_| config.work_dir.clone()),
            home: config.home.clone(),
            channel_id: config.channel_id(),
            admission: tokio::sync::Mutex::new(()),
            pending: Mutex::new(Vec::new()),
            wake: Notify::new(),
            talk: Mutex::default(),
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().expect("store mutex poisoned")
    }

    /// Startup recovery: interrupted rows become `needs_check` (never re-prompted) and the
    /// queued rows of their conversations are held, each conversation with unannounced
    /// rows gets one notice, rows that failed for their files get their pending notice, a
    /// reply whose file upload was cut gets one file notice (the file is skipped), stored
    /// replies are re-sent. A row cut off by a crash and one marked by a graceful shutdown
    /// are treated the same. A lane a restart interrupted gets one notice in its
    /// conversation. Last, an onboarding request gets its one greeting.
    pub async fn startup(&self) -> Result<(), StoreError> {
        let recovered = self.store().recover_on_start()?;
        self.post_needs_check_notices().await?;
        self.post_lane_notices().await?;
        self.post_failed_file_notices().await?;
        for id in recovered.interrupted_uploads {
            let Some(row) = self.store().get(id)? else {
                continue;
            };
            tracing::warn!(row = id, "file upload was cut by a restart, file skipped");
            if let Some(target) = reply_target(&row.conversation_key) {
                let mut notice_sent = row.notice_sent;
                self.file_not_sent(id, target, &mut notice_sent).await?;
            }
        }
        let starters = self.store().queued_thread_starters()?;
        for row in starters {
            if !self
                .store()
                .is_known_thread(&row.platform, &row.message_id)?
            {
                self.open_missing_thread(&row).await?;
            }
        }
        for row in recovered.reply_pending {
            self.send_reply(row.id).await?;
        }
        self.greet_if_requested().await
    }

    /// Starts the onboarding and posts [`N_ONBOARDING_HELLO`] once when a request file is
    /// present: in the owner's DM unless the request says `channel`, and in the configured
    /// channel when the DM cannot be opened or the send fails. The file is removed before the
    /// greeting; when it cannot be removed nothing is sent (no greeting at every start). A
    /// greeting that failed in both places is not retried.
    async fn greet_if_requested(&self) -> Result<(), StoreError> {
        let path = onboarding::request_file(&self.home);
        if std::fs::symlink_metadata(&path).is_err() {
            return Ok(());
        }
        let target = onboarding::read_request(&self.home);
        self.store().start_onboarding()?;
        if std::fs::remove_file(&path).is_err() {
            tracing::warn!(
                code = E_ONBOARDING_REQUEST,
                "onboarding request not removed; no greeting"
            );
            return Ok(());
        }
        let place = match target {
            GreetTarget::Dm if self.greet_in_dm().await => Some(GreetTarget::Dm),
            _ => self
                .notify(&self.channel_id, N_ONBOARDING_HELLO)
                .await
                .then_some(GreetTarget::Channel),
        };
        if let Some(place) = place {
            tracing::info!(place = place.as_str(), "onboarding greeting sent");
        }
        Ok(())
    }

    /// Opens the owner's DM and sends the greeting there; false when either step fails.
    async fn greet_in_dm(&self) -> bool {
        match self.adapter.open_dm(&self.owner_id).await {
            Ok(target) => self.notify(&target, N_ONBOARDING_HELLO).await,
            Err(err) => {
                tracing::warn!(code = err.code, "owner DM not opened for the greeting");
                false
            }
        }
    }

    /// Runs startup, then serves inbound events and the worker until `inbound` closes.
    pub async fn run(
        self: Arc<Self>,
        mut inbound: mpsc::Receiver<InboundEvent>,
    ) -> Result<(), StoreError> {
        let reconnects = self.adapter.reconnects();
        self.startup().await?;
        let worker = tokio::spawn({
            let this = Arc::clone(&self);
            async move { this.work_loop(reconnects).await }
        });
        let watchdog = tokio::spawn({
            let this = Arc::clone(&self);
            async move { this.omp.lanes().watch().await }
        });
        let news = tokio::spawn({
            let this = Arc::clone(&self);
            async move { this.news_loop().await }
        });
        while let Some(event) = inbound.recv().await {
            if let Err(err) = self.handle_event(event).await {
                tracing::error!(code = err.code(), error = %err, "inbound event not handled");
            }
        }
        worker.abort();
        watchdog.abort();
        news.abort();
        let _ = worker.await;
        let _ = watchdog.await;
        let _ = news.await;
        self.shutdown().await;
        Ok(())
    }

    /// Stops the lead child, even in the middle of a turn, and every lane child.
    pub async fn shutdown(&self) {
        self.omp.shutdown_all().await;
    }

    /// `Some(conversation key)` while a lead turn that accepts a steer runs.
    pub fn running_turn(&self) -> watch::Receiver<Option<String>> {
        self.omp.running_turn()
    }

    /// Serves the queue and re-sends failed replies after each adapter reconnect. Both run
    /// on this one task, so a reconnect seen during a turn is handled right after it, once
    /// the turn's failed send was remembered.
    async fn work_loop(&self, mut reconnects: Option<watch::Receiver<u64>>) {
        let lane_wake = self.omp.lane_wake();
        loop {
            if let Some(rx) = reconnects.as_mut()
                && rx.has_changed().unwrap_or(false)
            {
                rx.borrow_and_update();
                self.resend_after_reconnect().await;
            }
            let idle = match self.process_next().await {
                Ok(more) => !more,
                Err(err) => {
                    tracing::error!(code = err.code(), error = %err, "worker step failed");
                    true
                }
            };
            if idle {
                tokio::select! {
                    biased;
                    () = next_reconnect(&mut reconnects) => self.resend_after_reconnect().await,
                    () = self.wake.notified() => {}
                    () = lane_wake.notified() => {}
                }
            }
        }
    }

    async fn resend_after_reconnect(&self) {
        match self.resend_pending().await {
            Ok(delivered) => tracing::info!(delivered, "pending replies re-sent after reconnect"),
            Err(err) => {
                tracing::error!(code = err.code(), error = %err, "pending replies not re-sent")
            }
        }
    }

    /// Access check, bot reply limit, chat notice, input filter, file count, store-first
    /// admission, thread creation, then the attachment downloads outside the admission lock.
    pub async fn handle_event(&self, event: InboundEvent) -> Result<Handled, StoreError> {
        let sender = match self.access.allow(&event) {
            Decision::Allow(sender) => sender,
            Decision::Drop(reason) => {
                tracing::info!(
                    message_id = %event.message_id,
                    author_id = %event.author_id,
                    code = reason.code(),
                    "inbound message dropped"
                );
                return Ok(Handled::Dropped(reason));
            }
        };

        let platform = self.adapter.platform();
        let (key, new_thread) = match (&event.thread_id, event.is_dm) {
            (_, true) => (ConversationKey::dm(platform, &event.channel_id), false),
            (Some(thread), false) => (ConversationKey::thread(platform, thread), false),
            (None, false) => (ConversationKey::thread(platform, &event.message_id), true),
        };
        if event.author_is_bot
            && !self
                .bot_replies
                .lock()
                .expect("bot reply mutex poisoned")
                .take(key.as_str(), Instant::now())
        {
            tracing::info!(
                message_id = %event.message_id,
                author_id = %event.author_id,
                conversation = key.as_str(),
                code = DropReason::BotLimit.code(),
                "bot reply limit reached, message dropped"
            );
            return Ok(Handled::Dropped(DropReason::BotLimit));
        }

        let here = event.thread_id.as_deref().unwrap_or(&event.channel_id);
        if let Sender::Listed(role) = sender {
            tracing::info!(
                message_id = %event.message_id,
                author_id = %event.author_id,
                role = role.as_str(),
                code = N_CHAT_UNAVAILABLE,
                "chat message not run"
            );
            self.notify(here, N_CHAT_UNAVAILABLE).await;
            return Ok(Handled::Notice(N_CHAT_UNAVAILABLE));
        }
        let text = event.text.trim();
        if text.starts_with('/') || (text.is_empty() && event.attachments.is_empty()) {
            tracing::info!(message_id = %event.message_id, code = N_UNSUPPORTED_INPUT, "input not forwarded");
            self.notify(here, N_UNSUPPORTED_INPUT).await;
            return Ok(Handled::Notice(N_UNSUPPORTED_INPUT));
        }
        if event.attachments.len() > files::MAX_FILES {
            tracing::info!(
                message_id = %event.message_id,
                count = event.attachments.len(),
                code = files::E_FILE_TOO_MANY,
                "too many attachments, message not stored"
            );
            self.notify(here, N_FILE_NOT_RECEIVED).await;
            return Ok(Handled::FilesRejected(N_FILE_NOT_RECEIVED));
        }

        let attachments_json =
            (!event.attachments.is_empty()).then(|| declared_json(&event.attachments));
        let id = {
            let _gate = self.admission.lock().await;
            let admitted = self.store().admit_with_files(
                platform,
                &event.message_id,
                key.as_str(),
                &event.channel_id,
                &event.author_id,
                &event.text,
                attachments_json.as_deref(),
            )?;
            let id = match admitted {
                Admitted::Duplicate => {
                    tracing::info!(message_id = %event.message_id, "duplicate message ignored");
                    return Ok(Handled::Duplicate);
                }
                Admitted::New(id) => id,
            };
            self.talked(key.as_str());

            if new_thread {
                match self
                    .adapter
                    .create_thread(&event.channel_id, &event.message_id, &event.text)
                    .await
                {
                    Ok(thread) => self.store().remember_thread(platform, &thread)?,
                    Err(err) => {
                        self.thread_failed(id, err.code, &event.channel_id).await?;
                        return Ok(Handled::ThreadFailed(id));
                    }
                }
            }
            id
        };

        let mut saved: Vec<SavedFile> = Vec::new();
        if !event.attachments.is_empty() {
            let fetched = self.fetch_files(id, &key, &event.attachments).await;
            let _gate = self.admission.lock().await;
            match fetched {
                Ok(files) => {
                    let json = serde_json::to_string(&files)
                        .expect("saved-file metadata serializes to JSON");
                    self.store().files_saved(id, &json)?;
                    tracing::info!(row = id, count = files.len(), "attachments saved");
                    saved = files;
                }
                Err(err) => {
                    tracing::warn!(row = id, code = err.code, "attachments not received");
                    let notice = file_notice(err.code);
                    let failed = self.store().files_failed(id, err.code)?;
                    if failed
                        && let Some(target) = reply_target(key.as_str())
                        && self.notify(target, notice).await
                    {
                        self.store().mark_notice_sent(id)?;
                    }
                    return Ok(Handled::FilesRejected(notice));
                }
            }
        }
        // The same body the worker would send for this row.
        let body = lead::owner_input(
            key.as_str(),
            id,
            &files::prompt_with_files(&event.text, &saved),
        );
        let mut claim_error = None;
        let steered = self.omp.try_steer(key.as_str(), id, sender, &body, |main| {
            self.store().steer_into(id, main).unwrap_or_else(|err| {
                claim_error = Some(err);
                false
            })
        });
        if steered {
            return Ok(Handled::Steered(id));
        }
        tracing::info!(row = id, conversation = key.as_str(), "message queued");
        self.wake.notify_one();
        match claim_error {
            Some(err) => Err(err),
            None => Ok(Handled::Queued(id)),
        }
    }

    /// Fetches every attachment of row `id` into its own inbox folder, one after another,
    /// each under the stall watchdog with a fresh progress record. The folder is created
    /// here (0700 levels); adapters never create directories.
    async fn fetch_files(
        &self,
        id: i64,
        key: &ConversationKey,
        attachments: &[InboundAttachment],
    ) -> Result<Vec<SavedFile>, AdapterError> {
        let dir = files::inbox_dir(&self.work_dir, &key_slug(key.as_str()), id);
        files::create_private_dir_all(&dir)?;
        let mut saved = Vec::with_capacity(attachments.len());
        for (index, attachment) in attachments.iter().enumerate() {
            let name = files::sanitize_name(&attachment.name, index + 1);
            let path = dir.join(&name);
            let progress = files::Progress::new();
            let bytes = files::with_stall_timeout(
                self.adapter.fetch_attachment(attachment, &path, &progress),
                &progress,
            )
            .await?;
            tracing::info!(row = id, index, bytes, mime = ?attachment.mime, "attachment saved");
            saved.push(SavedFile {
                path,
                name,
                mime: attachment.mime.clone(),
                bytes,
            });
        }
        Ok(saved)
    }

    /// Serves the oldest queued row: claims it (a row a steer claimed first is skipped),
    /// runs one lead turn for it and settles the turn's group. Returns `false` when the
    /// queue is empty.
    pub async fn process_next(&self) -> Result<bool, StoreError> {
        let row = {
            let _gate = self.admission.lock().await;
            let Some(row) = self.store().next_queued()? else {
                return Ok(false);
            };
            if !self.store().claim(row.id)? {
                return Ok(true);
            }
            row
        };
        let key = row.conversation_key.as_str();
        let from_owner = row.origin == "owner";

        if from_owner && row.author_id != self.owner_id {
            tracing::warn!(
                row = row.id,
                code = E_ACCESS_NOT_OWNER,
                "stored author is not the owner"
            );
            self.store()
                .mark(row.id, State::Failed, Some(E_ACCESS_NOT_OWNER))?;
            return Ok(true);
        }
        let Some(target) = reply_target(key) else {
            self.store()
                .mark(row.id, State::Failed, Some(E_DISPATCH_KEY))?;
            return Ok(true);
        };
        if is_thread_starter(&row)
            && !self
                .store()
                .is_known_thread(&row.platform, &row.message_id)?
            && !self.open_missing_thread(&row).await?
        {
            return Ok(true);
        }

        let prompt = if from_owner {
            let saved: Vec<SavedFile> = match row.attachments.as_deref().map(serde_json::from_str) {
                None => Vec::new(),
                Some(Ok(saved)) => saved,
                Some(Err(_)) => {
                    tracing::warn!(
                        row = row.id,
                        code = files::E_FILE_INCOMPLETE,
                        "stored file list unreadable"
                    );
                    self.store()
                        .mark(row.id, State::Failed, Some(files::E_FILE_INCOMPLETE))?;
                    if self.notify(target, N_FILE_NOT_RECEIVED).await {
                        self.store().mark_notice_sent(row.id)?;
                    }
                    return Ok(true);
                }
            };
            let input =
                lead::owner_input(key, row.id, &files::prompt_with_files(&row.text, &saved));
            match self
                .store()
                .onboarding_turn(row.id)?
                .filter(|n| (1..=onboarding::MAX_TURNS).contains(n))
            {
                Some(n) => onboarding::mark_input(&input, n),
                None => input,
            }
        } else {
            // A lane result row stores the lead input `finish_lane` built.
            row.text.clone()
        };

        let typing = match self.adapter.start_typing(target).await {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::warn!(row = row.id, code = err.code, "typing indicator failed");
                None
            }
        };
        // Only owner rows are stored and run. A lane-result row acts for the owner who
        // started the lane.
        let group = self.omp.prompt(key, row.id, Sender::Owner, &prompt).await;
        drop(typing);
        self.settle_group(row.id, target, group).await?;
        Ok(true)
    }

    /// Stores the outcome of a lead turn and its steered row, if any: one reply on the
    /// main row (the steered row `done`), else every member `needs_check` with the
    /// conversation's notice, else every member `failed` with one notice.
    async fn settle_group(
        &self,
        main_id: i64,
        target: &str,
        group: GroupReport,
    ) -> Result<(), StoreError> {
        let GroupReport { main, steered } = group;
        let steered_id = steered.as_ref().map(|(id, _)| *id);
        let mut ids = vec![main_id];
        ids.extend(steered_id);
        let outcomes: Vec<&Outcome> = std::iter::once(&main.outcome)
            .chain(steered.as_ref().map(|(_, outcome)| outcome))
            .collect();

        if let Some(code) = outcomes.iter().find_map(|outcome| match outcome {
            Outcome::NeedsCheck { code, .. } => Some(*code),
            _ => None,
        }) {
            tracing::warn!(row = main_id, steered = ?steered_id, code, "turn interrupted");
            self.store().interrupt_group(&ids, code)?;
            self.post_needs_check_notices().await?;
            return Ok(());
        }
        if let Some(code) = outcomes.iter().find_map(|outcome| match outcome {
            Outcome::Failed { code, .. } => Some(*code),
            _ => None,
        }) {
            tracing::warn!(row = main_id, steered = ?steered_id, code, "turn failed");
            self.store().fail_group(&ids, code)?;
            let notice = if code == E_COST_CAP {
                N_COST_CAP
            } else {
                N_FAILED
            };
            self.notify(target, notice).await;
            return Ok(());
        }

        let reply = combine_replies(outcomes.iter().filter_map(|outcome| reply_text(outcome)));
        let out_files = out_files_json(main.files);
        self.store()
            .finish_group(main_id, &reply, out_files.as_deref(), steered_id)?;
        self.send_reply(main_id).await?;
        Ok(())
    }

    /// Opens the thread of a queued thread-starter row whose creation never completed.
    /// Returns whether the row may run; otherwise it is `failed` and announced.
    async fn open_missing_thread(&self, row: &InboxRow) -> Result<bool, StoreError> {
        let Some(channel) = self.store().channel_of(&row.conversation_key)? else {
            self.store()
                .mark(row.id, State::Failed, Some(E_DISPATCH_KEY))?;
            return Ok(false);
        };
        match self
            .adapter
            .create_thread(&channel, &row.message_id, &row.text)
            .await
        {
            Ok(thread) => {
                self.store().remember_thread(&row.platform, &thread)?;
                tracing::info!(row = row.id, "missing thread opened");
                Ok(true)
            }
            Err(err) => {
                self.thread_failed(row.id, err.code, &channel).await?;
                Ok(false)
            }
        }
    }

    /// Marks a row whose thread could not be opened `failed` and posts one notice in the
    /// channel; `notice_sent` records the delivery.
    async fn thread_failed(&self, id: i64, code: &str, channel: &str) -> Result<(), StoreError> {
        tracing::warn!(row = id, code, "thread creation failed");
        self.store().mark(id, State::Failed, Some(code))?;
        if self.notify(channel, N_THREAD_FAILED).await {
            self.store().mark_notice_sent(id)?;
        }
        Ok(())
    }

    /// Re-sends replies whose earlier send failed (call after a reconnect).
    /// Returns how many were delivered.
    pub async fn resend_pending(&self) -> Result<usize, StoreError> {
        let ids = std::mem::take(&mut *self.pending.lock().expect("pending lock"));
        let mut delivered = 0;
        for id in ids {
            let Some(row) = self.store().get(id)? else {
                continue;
            };
            if row.state != State::ReplyPending {
                continue;
            }
            if self.send_reply(row.id).await? {
                delivered += 1;
            }
        }
        Ok(delivered)
    }

    /// Sends a stored reply: its text chunks unless already delivered (`text_sent`), then
    /// each outbound file not yet sent or skipped. The row becomes `done` only when the text
    /// was accepted and every file is sent or skipped. A failed text send or a transient
    /// upload error keeps the row `reply_pending` and remembers it for a resend.
    async fn send_reply(&self, id: i64) -> Result<bool, StoreError> {
        let Some(row) = self.store().get(id)? else {
            return Ok(false);
        };
        let Some(target) = reply_target(&row.conversation_key) else {
            self.store().mark(id, State::Failed, Some(E_DISPATCH_KEY))?;
            return Ok(false);
        };
        if !row.text_sent {
            let reply = row.reply.as_deref().unwrap_or_default();
            for chunk in reply_messages(reply, self.adapter.message_limit()) {
                if let Err(err) = self.adapter.send(target, &chunk).await {
                    tracing::warn!(
                        row = id,
                        code = err.code,
                        "reply send failed, kept as reply_pending"
                    );
                    self.remember_pending(id);
                    return Ok(false);
                }
            }
            self.store().mark_text_sent(id)?;
            self.talked(&row.conversation_key);
        }

        let out_files: Vec<OutFile> = match row.out_files.as_deref().map(serde_json::from_str) {
            None => Vec::new(),
            Some(Ok(out_files)) => out_files,
            Some(Err(_)) => return Err(StoreError::Invalid("stored out-file list is not valid")),
        };
        let mut notice_sent = row.notice_sent;
        for (index, file) in out_files.iter().enumerate() {
            if file.sent || file.skipped {
                continue;
            }
            let path = match files::check_outbound(&file.path, &self.work_dir, &self.home) {
                Ok((path, _)) => path,
                Err(err) => {
                    tracing::warn!(row = id, index, code = err.code, "outbound file skipped");
                    self.store().mark_file_skipped(id, index)?;
                    self.file_not_sent(id, target, &mut notice_sent).await?;
                    continue;
                }
            };
            if file.attempts >= files::MAX_UPLOAD_ATTEMPTS {
                tracing::warn!(
                    row = id,
                    index,
                    attempts = file.attempts,
                    "outbound file skipped after its last attempt"
                );
                self.store().mark_file_skipped(id, index)?;
                self.file_not_sent(id, target, &mut notice_sent).await?;
                continue;
            }
            self.store().mark_file_attempting(id, index)?;
            let sent = tokio::time::timeout(
                files::UPLOAD_TIMEOUT,
                self.adapter.send_file(target, &path, &file.name),
            )
            .await;
            if let Ok(Ok(_)) = sent {
                self.store().mark_file_sent(id, index)?;
                tracing::info!(row = id, index, "outbound file sent");
                continue;
            }
            self.store().clear_file_attempting(id, index)?;
            let code = match &sent {
                Ok(Err(err)) => err.code,
                _ => files::E_FILE_UPLOAD_TIMEOUT,
            };
            tracing::warn!(row = id, index, code, "outbound file not sent");
            self.file_not_sent(id, target, &mut notice_sent).await?;
            if code == E_ADAPTER_UPLOAD_DENIED || code == files::E_FILE_UPLOAD_TIMEOUT {
                self.store().mark_file_skipped(id, index)?;
                continue;
            }
            self.remember_pending(id);
            return Ok(false);
        }
        self.store().mark_done(id)?;
        Ok(true)
    }

    /// Remembers a `reply_pending` row for the next reconnect resend.
    fn remember_pending(&self, id: i64) {
        let mut pending = self.pending.lock().expect("pending lock");
        if !pending.contains(&id) {
            pending.push(id);
        }
    }

    /// Posts the reply's one [`N_FILE_NOT_SENT`] unless it was already posted (`notice_sent`).
    async fn file_not_sent(
        &self,
        id: i64,
        target: &str,
        notice_sent: &mut bool,
    ) -> Result<(), StoreError> {
        if !*notice_sent && self.notify(target, N_FILE_NOT_SENT).await {
            self.store().mark_notice_sent(id)?;
            *notice_sent = true;
        }
        Ok(())
    }

    /// Posts the pending notice of each row that failed for its files: in its thread when
    /// that thread is known, else in the conversation's channel.
    async fn post_failed_file_notices(&self) -> Result<(), StoreError> {
        let rows = self.store().failed_notices()?;
        for row in rows {
            let error = row.error.as_deref().unwrap_or_default();
            let thread_known = match reply_target(&row.conversation_key) {
                Some(thread) => self.store().is_known_thread(&row.platform, thread)?,
                None => false,
            };
            let target = if thread_known {
                reply_target(&row.conversation_key).map(str::to_owned)
            } else {
                self.store().channel_of(&row.conversation_key)?
            };
            let Some(target) = target else {
                continue;
            };
            if self.notify(&target, file_notice(error)).await {
                self.store().mark_notice_sent(row.id)?;
            }
        }
        Ok(())
    }

    /// Posts one interrupted notice per conversation with unannounced `needs_check` rows.
    async fn post_needs_check_notices(&self) -> Result<(), StoreError> {
        let rows = self.store().needs_check_notices()?;
        let mut groups: Vec<(String, Vec<i64>)> = Vec::new();
        for row in rows {
            match groups
                .iter_mut()
                .find(|(key, _)| *key == row.conversation_key)
            {
                Some((_, ids)) => ids.push(row.id),
                None => groups.push((row.conversation_key, vec![row.id])),
            }
        }
        for (key, ids) in groups {
            let Some(target) = reply_target(&key) else {
                continue;
            };
            if self.notify(target, N_INTERRUPTED).await {
                let store = self.store();
                for id in ids {
                    store.mark_notice_sent(id)?;
                }
            }
        }
        Ok(())
    }

    /// Posts one [`N_LANE_INTERRUPTED`] per lane a restart interrupted, in the
    /// conversation that started it; `notice_sent` records the delivery.
    async fn post_lane_notices(&self) -> Result<(), StoreError> {
        let lanes = self.store().lane_notices()?;
        for lane in lanes {
            let Some(target) = reply_target(&lane.conversation_key) else {
                continue;
            };
            tracing::warn!(
                lane = lane.id,
                code = N_LANE_INTERRUPTED,
                "lane was interrupted by a restart"
            );
            if self.notify(target, N_LANE_INTERRUPTED).await {
                self.store().mark_lane_notice_sent(lane.id)?;
            }
        }
        Ok(())
    }

    /// Counts one owner message, delivered reply or posted news in `key`; no clock is read.
    fn talked(&self, key: &str) {
        self.talk
            .lock()
            .expect("talk lock")
            .entry(key.to_owned())
            .or_default()
            .said += 1;
    }

    /// Conversations whose talk is tracked for lane news; quiet ones are dropped at each news
    /// step.
    pub fn talk_len(&self) -> usize {
        self.talk.lock().expect("talk lock").len()
    }

    /// The lane manager, for lane news.
    pub fn lanes(&self) -> &LaneManager {
        self.omp.lanes()
    }

    /// One news step at tick time `now`: every conversation's quiet clock is updated, then
    /// each lane of `news` whose conversation is quiet for [`NEWS_AFTER`] gets its note
    /// delivered by [`Self::deliver_news`]; delivered texts are marked. Returns the number of
    /// notes the messenger accepted.
    pub async fn post_news(&self, news: Vec<LaneNews>, now: Instant) -> usize {
        let due: Vec<LaneNews> = {
            let mut talk = self.talk.lock().expect("talk lock");
            for t in talk.values_mut() {
                if t.seen.is_none_or(|(count, _)| count != t.said) {
                    t.seen = Some((t.said, now));
                }
            }
            talk.retain(|key, t| {
                news.iter().any(|n| n.conversation_key == *key)
                    || t.seen
                        .is_some_and(|(_, since)| now.saturating_duration_since(since) < NEWS_AFTER)
            });
            news.into_iter()
                .filter(|n| {
                    reply_target(&n.conversation_key).is_some()
                        && talk.get(&n.conversation_key).is_none_or(|t| {
                            t.seen.is_some_and(|(_, since)| {
                                now.saturating_duration_since(since) >= NEWS_AFTER
                            })
                        })
                })
                .collect()
        };

        let mut sent = 0;
        for n in due {
            let Some(target) = reply_target(&n.conversation_key) else {
                continue;
            };
            if self
                .deliver_news(target, n.lane_id, &news_line(n.lane_id, &n.text))
                .await
            {
                self.lanes().news_posted(n.lane_id, &n.text);
                self.talked(&n.conversation_key);
                sent += 1;
            }
        }
        sent
    }

    /// Edits the news message of `lane_id` to `line`, or posts `line` as a new message when
    /// the lane has none or the edit failed, and remembers the new id. Returns whether the
    /// messenger accepted it.
    async fn deliver_news(&self, target: &str, lane_id: i64, line: &str) -> bool {
        if let Some(message_id) = self.lanes().news_message(lane_id) {
            match self.adapter.edit(target, &message_id, line).await {
                Ok(()) => {
                    tracing::info!(lane = lane_id, "lane news edited");
                    return true;
                }
                Err(err) => {
                    tracing::warn!(lane = lane_id, code = err.code, "lane news edit failed")
                }
            }
        }
        match self.adapter.send(target, line).await {
            Ok(sent) => {
                self.lanes().set_news_message(lane_id, &sent.message_id);
                tracing::info!(lane = lane_id, "lane news posted");
                true
            }
            Err(err) => {
                tracing::warn!(lane = lane_id, code = err.code, "lane news send failed");
                false
            }
        }
    }

    /// Posts lane news after every lane watchdog tick until the task is aborted.
    async fn news_loop(&self) {
        let mut ticks = self.lanes().watch_ticks();
        while ticks.changed().await.is_ok() {
            let now = *ticks.borrow_and_update();
            if let Some(now) = now {
                self.post_news(self.lanes().news(), now).await;
            }
        }
    }

    /// Sends a fixed notice; returns whether the messenger accepted it.
    async fn notify(&self, channel: &str, code: &'static str) -> bool {
        match self.adapter.send(channel, notice_text(code)).await {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!(notice = code, code = err.code, "notice send failed");
                false
            }
        }
    }
}
