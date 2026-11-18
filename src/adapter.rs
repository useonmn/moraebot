use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use crate::files::Progress;

pub const E_ADAPTER_SEND: &str = "E_ADAPTER_SEND";
pub const E_ADAPTER_THREAD: &str = "E_ADAPTER_THREAD";
pub const E_ADAPTER_TYPING: &str = "E_ADAPTER_TYPING";
pub const E_ADAPTER_START: &str = "E_ADAPTER_START";
/// This messenger does not support files (the default of the file methods).
pub const E_ADAPTER_FILES: &str = "E_ADAPTER_FILES";
/// An upload failed in a way that may work next time (network, rate limit, 5xx).
pub const E_ADAPTER_UPLOAD: &str = "E_ADAPTER_UPLOAD";
/// The messenger permanently refused an upload (missing permission, file too large for
/// the messenger, unreadable file); the core skips the file.
pub const E_ADAPTER_UPLOAD_DENIED: &str = "E_ADAPTER_UPLOAD_DENIED";
/// The owner's DM could not be opened (the messenger refused, or it has no DMs).
pub const E_ADAPTER_DM: &str = "E_ADAPTER_DM";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundEvent {
    pub message_id: String,
    pub author_id: String,
    pub author_is_bot: bool,
    pub channel_id: String,
    pub thread_id: Option<String>,
    pub is_dm: bool,
    pub text: String,
    pub attachments: Vec<InboundAttachment>,
}

/// One file attached to an inbound message. Only the adapter that produced `reference`
/// can fetch it; an empty reference means the messenger cannot serve the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundAttachment {
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
    pub reference: AttachmentRef,
}

/// Opaque, messenger-specific pointer to a file. It may be a signed or token-gated URL,
/// so it never shows up in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct AttachmentRef(pub String);

impl std::fmt::Debug for AttachmentRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachmentRef(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConversationKey(String);

impl ConversationKey {
    pub fn thread(platform: &str, thread_id: &str) -> Self {
        Self(format!("{platform}:th:{thread_id}"))
    }

    pub fn dm(platform: &str, channel_id: &str) -> Self {
        Self(format!("{platform}:dm:{channel_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendResult {
    pub message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AdapterError {
    pub code: &'static str,
    pub message: String,
}

pub struct TypingHandle {
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl TypingHandle {
    pub fn new(stop: impl FnOnce() + Send + 'static) -> Self {
        Self {
            stop: Some(Box::new(stop)),
        }
    }
}

impl Drop for TypingHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

#[async_trait]
pub trait MessengerAdapter: Send + Sync {
    fn platform(&self) -> &'static str;

    /// The longest message, in characters, this messenger accepts in one send.
    fn message_limit(&self) -> usize;

    /// Built-in formatting hints for omp about this messenger (overridable per home).
    fn behavior_hints(&self) -> &'static str;

    async fn start(&self) -> Result<mpsc::Receiver<InboundEvent>, AdapterError>;

    async fn send(&self, channel: &str, text: &str) -> Result<SendResult, AdapterError>;

    async fn create_thread(
        &self,
        channel_id: &str,
        from_message_id: &str,
        name: &str,
    ) -> Result<String, AdapterError>;

    async fn start_typing(&self, channel: &str) -> Result<TypingHandle, AdapterError>;

    /// The send target of the bot's DM with `user_id`, opened when needed. Messengers
    /// without DMs keep this default.
    async fn open_dm(&self, _user_id: &str) -> Result<String, AdapterError> {
        Err(AdapterError {
            code: E_ADAPTER_DM,
            message: "this messenger cannot open a DM".to_owned(),
        })
    }

    /// Downloads `attachment` into `dest` with no size limit and returns the bytes written.
    /// Calls `progress.record(n)` after every chunk written so the core's stall watchdog
    /// sees the download moving. Must not follow redirects. Messengers without file
    /// support keep this default.
    async fn fetch_attachment(
        &self,
        _attachment: &InboundAttachment,
        _dest: &Path,
        _progress: &Progress,
    ) -> Result<u64, AdapterError> {
        Err(files_unsupported())
    }

    /// Reads the file at `path` whole and posts it in one upload into `target` (the same
    /// target format `send` takes). A permanent refusal is `E_ADAPTER_UPLOAD_DENIED`; any
    /// other code is transient. No timer of its own: the core bounds the whole call.
    async fn send_file(
        &self,
        _target: &str,
        _path: &Path,
        _name: &str,
    ) -> Result<SendResult, AdapterError> {
        Err(files_unsupported())
    }

    /// A counter that increases on every (re)connect of the messenger, so the core can
    /// re-send replies whose send failed. `None`: the adapter never reconnects.
    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        None
    }

    /// Closes the messenger connection after the gateway stopped.
    async fn shutdown(&self) {}
}

fn files_unsupported() -> AdapterError {
    AdapterError {
        code: E_ADAPTER_FILES,
        message: "this messenger does not support files".to_owned(),
    }
}

/// The next reconnect delay: `current` doubled, capped at `max`. `start` is the floor a
/// zero delay grows from.
pub fn next_backoff(current: Duration, start: Duration, max: Duration) -> Duration {
    (current.max(start) * 2).min(max)
}

/// Splits `text` into chunks of at most `limit` characters. A cut falls after the last
/// newline in the window, else after the last space, else at the limit. Joining the
/// chunks gives back `text` exactly.
pub fn split_reply(text: &str, limit: usize) -> Vec<String> {
    assert!(limit > 0, "split_reply limit must be positive");
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.chars().count() > limit {
        let window_end = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(index, _)| index);
        let window = &rest[..window_end];
        let cut = window
            .rfind('\n')
            .or_else(|| window.rfind(' '))
            .map_or(window_end, |index| index + 1);
        let (chunk, tail) = rest.split_at(cut);
        chunks.push(chunk.to_owned());
        rest = tail;
    }
    chunks.push(rest.to_owned());
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Call, FakeAdapter};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn split_keeps_chunks_within_limit_and_rejoins_exactly() {
        let text = "가나다라마바사아자차카타파하 ".repeat(40);
        let chunks = split_reply(&text, 25);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 25));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_prefers_newline_over_space() {
        let chunks = split_reply("aaaa bbbb\ncccc dddd", 12);
        assert_eq!(chunks, vec!["aaaa bbbb\n", "cccc dddd"]);
    }

    #[test]
    fn split_falls_back_to_space() {
        let chunks = split_reply("aaaa bbbb cccc", 9);
        assert_eq!(chunks, vec!["aaaa ", "bbbb cccc"]);
    }

    #[test]
    fn split_hard_cuts_a_long_word_at_limit() {
        let word = "x".repeat(5000);
        let chunks = split_reply(&word, 2000);
        let lengths: Vec<usize> = chunks.iter().map(|chunk| chunk.chars().count()).collect();
        assert_eq!(lengths, vec![2000, 2000, 1000]);
        assert_eq!(chunks.concat(), word);
    }

    #[test]
    fn split_counts_korean_by_characters() {
        assert_eq!(split_reply("가나다라마", 3), vec!["가나다", "라마"]);
    }

    #[tokio::test]
    async fn fake_records_calls_in_order_and_threads_use_message_id() {
        let fake = FakeAdapter::new();
        let thread = fake
            .create_thread("111", "1300000000000000001", "task")
            .await
            .unwrap();
        assert_eq!(thread, "1300000000000000001");
        fake.send(&thread, "hi").await.unwrap();
        let _typing = fake.start_typing(&thread).await.unwrap();
        assert_eq!(
            fake.calls(),
            vec![
                Call::CreateThread {
                    channel_id: "111".into(),
                    from_message_id: "1300000000000000001".into(),
                    name: "task".into(),
                },
                Call::Send {
                    channel: thread.clone(),
                    text: "hi".into(),
                },
                Call::StartTyping { channel: thread },
            ]
        );
    }

    #[tokio::test]
    async fn injected_send_failure_returns_code_and_records_attempt() {
        let fake = FakeAdapter::new();
        fake.fail_next_send();
        let err = fake.send("thread-x", "hi").await.unwrap_err();
        assert_eq!(err.code, E_ADAPTER_SEND);
        assert_eq!(fake.calls().len(), 1);
        fake.send("thread-x", "again").await.unwrap();
        assert_eq!(fake.calls().len(), 2);
    }

    #[tokio::test]
    async fn wait_for_send_resolves_at_the_nth_send() {
        let fake = Arc::new(FakeAdapter::new());
        let waiter = {
            let fake = fake.clone();
            tokio::spawn(async move { fake.wait_for_send(2).await })
        };
        fake.send("t", "one").await.unwrap();
        fake.send("t", "two").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("wait_for_send timed out")
            .expect("waiter task panicked");
    }

    #[tokio::test]
    async fn injected_events_reach_the_core_receiver() {
        let fake = FakeAdapter::new();
        let mut rx = fake.start().await.unwrap();
        fake.inject(InboundEvent {
            message_id: "1".into(),
            author_id: "111".into(),
            author_is_bot: false,
            channel_id: "222".into(),
            thread_id: None,
            is_dm: false,
            text: "hello".into(),
            attachments: Vec::new(),
        })
        .await;
        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("event timed out")
            .expect("channel closed");
        assert_eq!(event.text, "hello");
    }

    #[test]
    fn attachment_ref_debug_is_redacted() {
        let event = InboundEvent {
            message_id: "1".into(),
            author_id: "111".into(),
            author_is_bot: false,
            channel_id: "222".into(),
            thread_id: None,
            is_dm: false,
            text: String::new(),
            attachments: vec![InboundAttachment {
                name: "a.txt".into(),
                size: 1,
                mime: None,
                reference: AttachmentRef("https://example.invalid/SENTINEL".into()),
            }],
        };
        let shown = format!("{event:?}");
        assert!(!shown.contains("SENTINEL") && !shown.contains("example.invalid"));
    }

    struct Minimal;

    #[async_trait]
    impl MessengerAdapter for Minimal {
        fn platform(&self) -> &'static str {
            "minimal"
        }
        fn message_limit(&self) -> usize {
            10
        }
        fn behavior_hints(&self) -> &'static str {
            ""
        }
        async fn start(&self) -> Result<mpsc::Receiver<InboundEvent>, AdapterError> {
            unreachable!()
        }
        async fn send(&self, _: &str, _: &str) -> Result<SendResult, AdapterError> {
            unreachable!()
        }
        async fn create_thread(&self, _: &str, _: &str, _: &str) -> Result<String, AdapterError> {
            unreachable!()
        }
        async fn start_typing(&self, _: &str) -> Result<TypingHandle, AdapterError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn default_open_dm_is_unsupported() {
        assert_eq!(Minimal.open_dm("111").await.unwrap_err().code, E_ADAPTER_DM);
    }

    #[tokio::test]
    async fn default_file_methods_return_files_unsupported() {
        let attachment = InboundAttachment {
            name: "a".into(),
            size: 0,
            mime: None,
            reference: AttachmentRef(String::new()),
        };
        let progress = Progress::new();
        let fetch = Minimal
            .fetch_attachment(&attachment, Path::new("x"), &progress)
            .await
            .unwrap_err();
        assert_eq!(fetch.code, E_ADAPTER_FILES);
        let send = Minimal
            .send_file("t", Path::new("x"), "x")
            .await
            .unwrap_err();
        assert_eq!(send.code, E_ADAPTER_FILES);
    }
}
