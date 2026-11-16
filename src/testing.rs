use crate::adapter::{
    AdapterError, E_ADAPTER_SEND, InboundEvent, MessengerAdapter, SendResult, TypingHandle,
};
use async_trait::async_trait;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
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
