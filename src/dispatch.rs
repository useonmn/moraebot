//! Core dispatcher: inbound event -> owner check -> store -> one global worker -> omp -> reply.
//!
//! Order of an inbound message: drop non-owner and bot authors (nothing stored), answer
//! empty, attachment-only and `/` messages with one fixed notice (never stored, never
//! forwarded), pick the conversation key, store the row FIRST (a duplicate stops here),
//! and only then open a thread for a message posted directly in the channel.
//!
//! A row that starts its own thread (key `<platform>:th:<its own message id>`) is run only
//! once that thread is known (`known_threads`). A row left without a thread by a crash
//! between admission and thread creation gets one more creation attempt at startup (or
//! at dequeue); if that fails it becomes `failed` with one notice in the channel, and it
//! never reaches omp.
//!
//! One worker serves the global FIFO with one message in flight. A reply is stored
//! (`reply_pending`) before it is sent and the row becomes `done` only after every chunk
//! was accepted; a failed send is retried after the adapter reconnects and at startup.
//! Interrupted work (`needs_check`) is never re-prompted; its conversation gets one
//! notice, also across restarts (`notice_sent`), and its older queued rows are held: they
//! are marked `needs_check` / [`E_DISPATCH_HELD`] in the same store transaction as the
//! interruption, so the hold survives a restart.

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Notify, mpsc, watch};

use crate::access::{self, Decision, DropReason, E_ACCESS_NOT_OWNER};
use crate::adapter::{ConversationKey, InboundEvent, MessengerAdapter, split_reply};
use crate::config::Config;
use crate::omp::{E_COST_CAP, OmpManager, Outcome};
pub use crate::store::E_DISPATCH_HELD;
use crate::store::{Admitted, InboxRow, State, Store, StoreError};

/// Empty, attachment-only or `/` message: not forwarded to omp.
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

/// Stored row error: the conversation key has no reply target.
pub const E_DISPATCH_KEY: &str = "E_DISPATCH_KEY";

/// Longer replies are cut with a note.
pub const MAX_REPLY_MESSAGES: usize = 10;

/// The fixed English text of a notice code.
pub fn notice_text(code: &str) -> &'static str {
    match code {
        N_UNSUPPORTED_INPUT => {
            "Only plain text messages are sent to the agent. Messages starting with \"/\", \
             empty messages and attachments are not forwarded."
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

/// Whether the row opened its own thread: its key is `<platform>:th:<its message id>`.
fn is_thread_starter(row: &InboxRow) -> bool {
    row.conversation_key == ConversationKey::thread(&row.platform, &row.message_id).as_str()
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
    Queued(i64),
}

pub struct Dispatcher {
    owner_id: String,
    adapter: Arc<dyn MessengerAdapter>,
    store: Arc<Mutex<Store>>,
    omp: OmpManager,
    /// Held while a row is admitted and its thread opened, and while the worker dequeues,
    /// so the worker never picks a row whose thread does not exist yet.
    admission: tokio::sync::Mutex<()>,
    /// Rows whose reply send failed in this process; re-sent by [`Self::resend_pending`].
    pending: Mutex<Vec<i64>>,
    wake: Notify,
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
            adapter,
            store,
            omp,
            admission: tokio::sync::Mutex::new(()),
            pending: Mutex::new(Vec::new()),
            wake: Notify::new(),
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().expect("store mutex poisoned")
    }

    /// Startup recovery: interrupted rows become `needs_check` (never re-prompted) and the
    /// queued rows of their conversations are held, each conversation with unannounced
    /// rows gets one notice, stored replies are re-sent. A row cut off by a crash and one
    /// marked by a graceful shutdown are treated the same.
    pub async fn startup(&self) -> Result<(), StoreError> {
        let recovered = self.store().recover_on_start()?;
        self.post_needs_check_notices().await?;
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
            self.send_reply(
                row.id,
                &row.conversation_key,
                row.reply.as_deref().unwrap_or_default(),
            )
            .await?;
        }
        Ok(())
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
        while let Some(event) = inbound.recv().await {
            if let Err(err) = self.handle_event(event).await {
                tracing::error!(code = err.code(), error = %err, "inbound event not handled");
            }
        }
        worker.abort();
        let _ = worker.await;
        self.shutdown().await;
        Ok(())
    }

    /// Closes the live omp child, if any.
    pub async fn shutdown(&self) {
        self.omp.shutdown_all().await;
    }

    /// Serves the queue and re-sends failed replies after each adapter reconnect. Both run
    /// on this one task, so a reconnect seen during a turn is handled right after it, once
    /// the turn's failed send was remembered.
    async fn work_loop(&self, mut reconnects: Option<watch::Receiver<u64>>) {
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

    /// Owner check, input filter, conversation key, store-first admission, thread creation.
    pub async fn handle_event(&self, event: InboundEvent) -> Result<Handled, StoreError> {
        if let Decision::Drop(reason) = access::allow(&event, &self.owner_id) {
            tracing::info!(
                message_id = %event.message_id,
                author_id = %event.author_id,
                code = reason.code(),
                "inbound message dropped"
            );
            return Ok(Handled::Dropped(reason));
        }

        let here = event.thread_id.as_deref().unwrap_or(&event.channel_id);
        let text = event.text.trim();
        if text.is_empty() || text.starts_with('/') {
            tracing::info!(message_id = %event.message_id, code = N_UNSUPPORTED_INPUT, "input not forwarded");
            self.notify(here, N_UNSUPPORTED_INPUT).await;
            return Ok(Handled::Notice(N_UNSUPPORTED_INPUT));
        }

        let platform = self.adapter.platform();
        let (key, new_thread) = match (&event.thread_id, event.is_dm) {
            (_, true) => (ConversationKey::dm(platform, &event.channel_id), false),
            (Some(thread), false) => (ConversationKey::thread(platform, thread), false),
            (None, false) => (ConversationKey::thread(platform, &event.message_id), true),
        };

        let _gate = self.admission.lock().await;
        let admitted = self.store().admit(
            platform,
            &event.message_id,
            key.as_str(),
            &event.channel_id,
            &event.author_id,
            &event.text,
        )?;
        let id = match admitted {
            Admitted::Duplicate => {
                tracing::info!(message_id = %event.message_id, "duplicate message ignored");
                return Ok(Handled::Duplicate);
            }
            Admitted::New(id) => id,
        };

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
        tracing::info!(row = id, conversation = key.as_str(), "message queued");
        self.wake.notify_one();
        Ok(Handled::Queued(id))
    }

    /// Serves the oldest queued row. Returns `false` when the queue is empty.
    pub async fn process_next(&self) -> Result<bool, StoreError> {
        let row = {
            let _gate = self.admission.lock().await;
            self.store().next_queued()?
        };
        let Some(row) = row else {
            return Ok(false);
        };
        let key = row.conversation_key.as_str();

        if row.author_id != self.owner_id {
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

        self.store().mark(row.id, State::Processing, None)?;
        let typing = match self.adapter.start_typing(target).await {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::warn!(row = row.id, code = err.code, "typing indicator failed");
                None
            }
        };
        let outcome = self.omp.prompt(key, &row.text).await;
        drop(typing);

        match outcome {
            Outcome::Reply(reply) => {
                self.store().save_reply(row.id, &reply)?;
                self.send_reply(row.id, key, &reply).await?;
            }
            Outcome::NoAgent => {
                let reply = notice_text(N_NO_AGENT);
                self.store().save_reply(row.id, reply)?;
                self.send_reply(row.id, key, reply).await?;
            }
            Outcome::Failed { code, .. } => {
                tracing::warn!(row = row.id, code, "turn failed");
                self.store().mark(row.id, State::Failed, Some(code))?;
                let notice = if code == E_COST_CAP {
                    N_COST_CAP
                } else {
                    N_FAILED
                };
                self.notify(target, notice).await;
            }
            Outcome::NeedsCheck { code, .. } => {
                tracing::warn!(row = row.id, code, "turn interrupted");
                self.store().mark_interrupted(row.id, code)?;
                self.post_needs_check_notices().await?;
            }
        }
        Ok(true)
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
            let reply = row.reply.unwrap_or_default();
            if self
                .send_reply(row.id, &row.conversation_key, &reply)
                .await?
            {
                delivered += 1;
            }
        }
        Ok(delivered)
    }

    /// Sends a stored reply; the row becomes `done` only after every chunk was accepted.
    /// On failure the row stays `reply_pending` and is remembered for a resend.
    async fn send_reply(&self, id: i64, key: &str, reply: &str) -> Result<bool, StoreError> {
        let Some(target) = reply_target(key) else {
            self.store().mark(id, State::Failed, Some(E_DISPATCH_KEY))?;
            return Ok(false);
        };
        for chunk in reply_messages(reply, self.adapter.message_limit()) {
            if let Err(err) = self.adapter.send(target, &chunk).await {
                tracing::warn!(
                    row = id,
                    code = err.code,
                    "reply send failed, kept as reply_pending"
                );
                let mut pending = self.pending.lock().expect("pending lock");
                if !pending.contains(&id) {
                    pending.push(id);
                }
                return Ok(false);
            }
        }
        self.store().mark_done(id)?;
        Ok(true)
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
