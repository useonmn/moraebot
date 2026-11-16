use crate::adapter::{
    AdapterError, E_ADAPTER_SEND, InboundEvent, MessengerAdapter, SendResult, TypingHandle,
};
use crate::slack::{
    E_SLACK_API, E_SLACK_AUTH, PostOutcome, SlackSocket, SlackTransport, SocketMessage,
};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Send {
        channel: String,
        text: String,
    },
    CreateThread {
        channel_id: String,
        from_message_id: String,
        name: String,
    },
    StartTyping {
        channel: String,
    },
}

pub struct FakeAdapter {
    calls: Mutex<Vec<Call>>,
    sent_count: watch::Sender<usize>,
    fail_next_send: AtomicBool,
    reconnects: watch::Sender<u64>,
    inbound_tx: mpsc::Sender<InboundEvent>,
    inbound_rx: Mutex<Option<mpsc::Receiver<InboundEvent>>>,
}

impl Default for FakeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeAdapter {
    pub fn new() -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel(64);
        Self {
            calls: Mutex::new(Vec::new()),
            sent_count: watch::channel(0).0,
            fail_next_send: AtomicBool::new(false),
            reconnects: watch::channel(0).0,
            inbound_tx,
            inbound_rx: Mutex::new(Some(inbound_rx)),
        }
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("fake adapter calls lock").clone()
    }

    pub fn fail_next_send(&self) {
        self.fail_next_send.store(true, Ordering::SeqCst);
    }

    /// Simulates the messenger connection coming back (a ready or resume event).
    pub fn reconnect(&self) {
        self.reconnects.send_modify(|count| *count += 1);
    }

    pub async fn inject(&self, event: InboundEvent) {
        self.inbound_tx
            .send(event)
            .await
            .expect("inbound receiver was dropped");
    }

    pub async fn wait_for_send(&self, n: usize) {
        self.sent_count
            .subscribe()
            .wait_for(|count| *count >= n)
            .await
            .map(|_| ())
            .expect("fake adapter owns the sender");
    }
}

#[async_trait]
impl MessengerAdapter for FakeAdapter {
    fn platform(&self) -> &'static str {
        "fake"
    }

    fn message_limit(&self) -> usize {
        2000
    }

    fn behavior_hints(&self) -> &'static str {
        "fake hints"
    }

    async fn start(&self) -> Result<mpsc::Receiver<InboundEvent>, AdapterError> {
        Ok(self
            .inbound_rx
            .lock()
            .expect("fake adapter receiver lock")
            .take()
            .expect("fake adapter started twice"))
    }

    async fn send(&self, channel: &str, text: &str) -> Result<SendResult, AdapterError> {
        let attempt = {
            let mut calls = self.calls.lock().expect("fake adapter calls lock");
            calls.push(Call::Send {
                channel: channel.to_owned(),
                text: text.to_owned(),
            });
            let attempt = calls
                .iter()
                .filter(|call| matches!(call, Call::Send { .. }))
                .count();
            self.sent_count.send_replace(attempt);
            attempt
        };
        if self.fail_next_send.swap(false, Ordering::SeqCst) {
            return Err(AdapterError {
                code: E_ADAPTER_SEND,
                message: "injected send failure".to_owned(),
            });
        }
        Ok(SendResult {
            message_id: format!("fake-msg-{attempt}"),
        })
    }

    async fn create_thread(
        &self,
        channel_id: &str,
        from_message_id: &str,
        name: &str,
    ) -> Result<String, AdapterError> {
        self.calls
            .lock()
            .expect("fake adapter calls lock")
            .push(Call::CreateThread {
                channel_id: channel_id.to_owned(),
                from_message_id: from_message_id.to_owned(),
                name: name.to_owned(),
            });
        Ok(from_message_id.to_owned())
    }

    async fn start_typing(&self, channel: &str) -> Result<TypingHandle, AdapterError> {
        self.calls
            .lock()
            .expect("fake adapter calls lock")
            .push(Call::StartTyping {
                channel: channel.to_owned(),
            });
        Ok(TypingHandle::new(|| {}))
    }

    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        Some(self.reconnects.subscribe())
    }
}

/// What `FakeSlackTransport` saw, in call order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlackRecord {
    pub auth_tests: usize,
    pub opens: usize,
    /// URLs passed to `connect`, one per socket session.
    pub connected_urls: Vec<String>,
    /// Every `chat.postMessage` body, rate-limited and failed attempts included.
    pub posts: Vec<Value>,
    /// Every text the adapter wrote on any socket (the acks).
    pub socket_texts: Vec<String>,
    pub pings: usize,
    pub closed_sockets: usize,
}

type SocketItem = Result<SocketMessage, AdapterError>;

#[derive(Default)]
struct FakeSlackState {
    record: SlackRecord,
    fail_auth: bool,
    rate_limits: VecDeque<Duration>,
    fail_next_post: bool,
    sessions: Vec<mpsc::UnboundedSender<SocketItem>>,
}

struct FakeSlackShared {
    state: Mutex<FakeSlackState>,
    changed: watch::Sender<u64>,
}

impl FakeSlackShared {
    fn update<T>(&self, change: impl FnOnce(&mut FakeSlackState) -> T) -> T {
        let result = change(&mut self.state.lock().expect("fake slack state lock"));
        self.changed.send_modify(|count| *count += 1);
        result
    }
}

/// Offline Slack: records every call, and each `connect` opens a scripted socket session
/// the test drives through `session(n)`. Nothing reaches the network.
pub struct FakeSlackTransport {
    shared: Arc<FakeSlackShared>,
}

impl Default for FakeSlackTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSlackTransport {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(FakeSlackShared {
                state: Mutex::new(FakeSlackState::default()),
                changed: watch::channel(0).0,
            }),
        }
    }

    /// Every later `auth_test` fails as Slack does for a bad bot token.
    pub fn fail_auth(&self) {
        self.shared.update(|state| state.fail_auth = true);
    }

    /// The next `post_message` answers HTTP 429 with this Retry-After. Calls queue up.
    pub fn rate_limit_next(&self, wait: Duration) {
        self.shared
            .update(|state| state.rate_limits.push_back(wait));
    }

    /// The next `post_message` fails with a Slack API error.
    pub fn fail_next_post(&self) {
        self.shared.update(|state| state.fail_next_post = true);
    }

    pub fn record(&self) -> SlackRecord {
        self.shared
            .state
            .lock()
            .expect("fake slack state lock")
            .record
            .clone()
    }

    /// Resolves once `done` holds for the record. Wrap it in a timeout.
    pub async fn wait_until(&self, done: impl Fn(&SlackRecord) -> bool) {
        let mut changed = self.shared.changed.subscribe();
        loop {
            if done(&self.record()) {
                return;
            }
            changed
                .changed()
                .await
                .expect("fake slack transport owns the sender");
        }
    }

    /// The `n`th socket session (1-based), once the adapter has connected it.
    pub async fn session(&self, n: usize) -> FakeSlackSession {
        let mut changed = self.shared.changed.subscribe();
        loop {
            if let Some(tx) = self
                .shared
                .state
                .lock()
                .expect("fake slack state lock")
                .sessions
                .get(n - 1)
            {
                return FakeSlackSession { tx: tx.clone() };
            }
            changed
                .changed()
                .await
                .expect("fake slack transport owns the sender");
        }
    }
}

/// The test's end of one fake socket session.
#[derive(Clone)]
pub struct FakeSlackSession {
    tx: mpsc::UnboundedSender<SocketItem>,
}

impl FakeSlackSession {
    fn push(&self, item: SocketItem) {
        // The adapter may already have dropped this session; that is what some tests check.
        let _ = self.tx.send(item);
    }

    pub fn frame(&self, frame: Value) {
        self.push(Ok(SocketMessage::Text(frame.to_string())));
    }

    pub fn pong(&self) {
        self.push(Ok(SocketMessage::Pong));
    }

    pub fn close(&self) {
        self.push(Ok(SocketMessage::Close));
    }

    pub fn error(&self) {
        self.push(Err(AdapterError {
            code: E_SLACK_API,
            message: "injected socket error".to_owned(),
        }));
    }
}

pub struct FakeSlackSocket {
    rx: mpsc::UnboundedReceiver<SocketItem>,
    shared: Arc<FakeSlackShared>,
}

#[async_trait]
impl SlackSocket for FakeSlackSocket {
    async fn next(&mut self) -> Option<SocketItem> {
        self.rx.recv().await
    }

    async fn send_text(&mut self, text: String) -> Result<(), AdapterError> {
        self.shared
            .update(|state| state.record.socket_texts.push(text));
        Ok(())
    }

    async fn ping(&mut self) -> Result<(), AdapterError> {
        self.shared.update(|state| state.record.pings += 1);
        Ok(())
    }

    async fn close(&mut self) {
        self.shared.update(|state| state.record.closed_sockets += 1);
    }
}

#[async_trait]
impl SlackTransport for FakeSlackTransport {
    async fn auth_test(&self) -> Result<String, AdapterError> {
        self.shared.update(|state| {
            state.record.auth_tests += 1;
            if state.fail_auth {
                Err(AdapterError {
                    code: E_SLACK_AUTH,
                    message: "invalid_auth".to_owned(),
                })
            } else {
                Ok("UBOT".to_owned())
            }
        })
    }

    async fn open_connection(&self) -> Result<String, AdapterError> {
        self.shared.update(|state| {
            state.record.opens += 1;
            Ok(format!("wss://fake.test/link/{}", state.record.opens))
        })
    }

    async fn connect(&self, url: &str) -> Result<Box<dyn SlackSocket>, AdapterError> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared.update(|state| {
            state.record.connected_urls.push(url.to_owned());
            state.sessions.push(tx);
        });
        Ok(Box::new(FakeSlackSocket {
            rx,
            shared: self.shared.clone(),
        }))
    }

    async fn post_message(&self, body: Value) -> Result<PostOutcome, AdapterError> {
        self.shared.update(|state| {
            state.record.posts.push(body);
            if let Some(wait) = state.rate_limits.pop_front() {
                return Ok(PostOutcome::RateLimited(wait));
            }
            if std::mem::take(&mut state.fail_next_post) {
                return Err(AdapterError {
                    code: E_SLACK_API,
                    message: "channel_not_found".to_owned(),
                });
            }
            Ok(PostOutcome::Posted(format!(
                "{}.0001",
                state.record.posts.len()
            )))
        })
    }
}
