use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

pub const E_ADAPTER_SEND: &str = "E_ADAPTER_SEND";
pub const E_ADAPTER_THREAD: &str = "E_ADAPTER_THREAD";
pub const E_ADAPTER_TYPING: &str = "E_ADAPTER_TYPING";
pub const E_ADAPTER_START: &str = "E_ADAPTER_START";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundEvent {
    pub message_id: String,
    pub author_id: String,
    pub author_is_bot: bool,
    pub channel_id: String,
    pub thread_id: Option<String>,
    pub is_dm: bool,
    pub text: String,
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

    /// A counter that increases on every (re)connect of the messenger, so the core can
    /// re-send replies whose send failed. `None`: the adapter never reconnects.
    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        None
    }

    /// Closes the messenger connection after the gateway stopped.
    async fn shutdown(&self) {}
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
        let thread = fake.create_thread("111", "1300000000000000001", "task").await.unwrap();
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
        })
        .await;
        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("event timed out")
            .expect("channel closed");
        assert_eq!(event.text, "hello");
    }
}
