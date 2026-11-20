use crate::adapter::E_ADAPTER_EDIT;
use crate::adapter::{
    AdapterError, E_ADAPTER_DM, E_ADAPTER_SEND, E_ADAPTER_UPLOAD_DENIED, InboundAttachment,
    InboundEvent, MessengerAdapter, SendResult, TypingHandle,
};
use crate::files::{E_FILE_WRITE, Progress};
use crate::slack::{
    E_SLACK_API, E_SLACK_AUTH, PostOutcome, SlackSocket, SlackTransport, SocketMessage,
    upload_error_code, upload_status_code,
};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, watch};

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
    OpenDm {
        user_id: String,
    },
    FetchAttachment {
        reference: String,
        dest: PathBuf,
    },
    SendFile {
        target: String,
        name: String,
        bytes: u64,
    },
    Edit {
        channel: String,
        message_id: String,
        text: String,
    },
}

/// The test's end of a streamed fetch: send chunks, read the running byte total, drop
/// `chunks` to finish the fetch.
pub struct FetchFeed {
    pub chunks: mpsc::Sender<Vec<u8>>,
    pub written: mpsc::UnboundedReceiver<u64>,
}

enum FetchMode {
    Hold(Arc<Notify>),
    Stream {
        chunks: mpsc::Receiver<Vec<u8>>,
        written: mpsc::UnboundedSender<u64>,
    },
}

#[derive(Default)]
struct FileScript {
    attachment_bytes: HashMap<String, Vec<u8>>,
    fetch_failures: VecDeque<&'static str>,
    send_failures: VecDeque<&'static str>,
    fetch_modes: VecDeque<FetchMode>,
    send_holds: VecDeque<Arc<Notify>>,
}

pub struct FakeAdapter {
    calls: Mutex<Vec<Call>>,
    call_count: watch::Sender<usize>,
    files: Mutex<FileScript>,
    sent_count: watch::Sender<usize>,
    fail_next_send: AtomicBool,
    fail_next_open_dm: AtomicBool,
    fail_next_edit: AtomicBool,
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
            call_count: watch::channel(0).0,
            files: Mutex::new(FileScript::default()),
            sent_count: watch::channel(0).0,
            fail_next_send: AtomicBool::new(false),
            fail_next_open_dm: AtomicBool::new(false),
            fail_next_edit: AtomicBool::new(false),
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

    /// The next `open_dm` fails with `E_ADAPTER_DM`.
    pub fn fail_next_open_dm(&self) {
        self.fail_next_open_dm.store(true, Ordering::SeqCst);
    }

    /// The next `edit` records its call, then fails with `E_ADAPTER_EDIT`.
    pub fn fail_next_edit(&self) {
        self.fail_next_edit.store(true, Ordering::SeqCst);
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

    /// The bytes `fetch_attachment` writes for an attachment with this reference.
    pub fn set_attachment_bytes(&self, reference: &str, bytes: Vec<u8>) {
        self.script()
            .attachment_bytes
            .insert(reference.to_owned(), bytes);
    }

    /// The next `fetch_attachment` fails with `code` (calls queue up).
    pub fn fail_next_fetch(&self, code: &'static str) {
        self.script().fetch_failures.push_back(code);
    }

    /// The next `send_file` fails with `code` after recording its call (calls queue up).
    pub fn fail_next_send_file(&self, code: &'static str) {
        self.script().send_failures.push_back(code);
    }

    /// The next `fetch_attachment` records its call, then waits for the returned `Notify`
    /// before returning its scripted result. It records no progress while it waits.
    pub fn hold_next_fetch(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.script()
            .fetch_modes
            .push_back(FetchMode::Hold(gate.clone()));
        gate
    }

    /// The next `fetch_attachment` writes every chunk the test sends, records progress per
    /// chunk and returns the total once the sender is dropped.
    pub fn stream_next_fetch(&self) -> FetchFeed {
        let (chunks_tx, chunks) = mpsc::channel(1);
        let (written, written_rx) = mpsc::unbounded_channel();
        self.script()
            .fetch_modes
            .push_back(FetchMode::Stream { chunks, written });
        FetchFeed {
            chunks: chunks_tx,
            written: written_rx,
        }
    }

    /// The next `send_file` records its call, then waits for the returned `Notify` and
    /// returns `Ok`. Never notified: it never completes.
    pub fn hold_next_send_file(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.script().send_holds.push_back(gate.clone());
        gate
    }

    /// Resolves once a recorded call satisfies `pred`. Wrap it in a timeout.
    pub async fn wait_for_call(&self, pred: impl Fn(&Call) -> bool) {
        let mut changed = self.call_count.subscribe();
        loop {
            if self.calls().iter().any(&pred) {
                return;
            }
            changed
                .changed()
                .await
                .expect("fake adapter owns the sender");
        }
    }

    fn script(&self) -> std::sync::MutexGuard<'_, FileScript> {
        self.files.lock().expect("fake adapter files lock")
    }

    fn record(&self, call: Call) {
        self.calls
            .lock()
            .expect("fake adapter calls lock")
            .push(call);
        self.call_count.send_modify(|count| *count += 1);
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
            self.record(Call::Send {
                channel: channel.to_owned(),
                text: text.to_owned(),
            });
            let attempt = self
                .calls()
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
        self.record(Call::CreateThread {
            channel_id: channel_id.to_owned(),
            from_message_id: from_message_id.to_owned(),
            name: name.to_owned(),
        });
        Ok(from_message_id.to_owned())
    }

    async fn start_typing(&self, channel: &str) -> Result<TypingHandle, AdapterError> {
        self.record(Call::StartTyping {
            channel: channel.to_owned(),
        });
        Ok(TypingHandle::new(|| {}))
    }

    /// The DM target is `dm-<user_id>`.
    async fn open_dm(&self, user_id: &str) -> Result<String, AdapterError> {
        self.record(Call::OpenDm {
            user_id: user_id.to_owned(),
        });
        if self.fail_next_open_dm.swap(false, Ordering::SeqCst) {
            return Err(AdapterError {
                code: E_ADAPTER_DM,
                message: "injected open_dm failure".to_owned(),
            });
        }
        Ok(format!("dm-{user_id}"))
    }

    async fn fetch_attachment(
        &self,
        attachment: &InboundAttachment,
        dest: &Path,
        progress: &Progress,
    ) -> Result<u64, AdapterError> {
        self.record(Call::FetchAttachment {
            reference: attachment.reference.0.clone(),
            dest: dest.to_owned(),
        });
        let mode = self.script().fetch_modes.pop_front();
        if let Some(FetchMode::Stream {
            mut chunks,
            written,
        }) = mode
        {
            let mut file = create_private_file(dest)?;
            let mut total = 0u64;
            while let Some(chunk) = chunks.recv().await {
                if let Err(err) = progress.check_limit(total + chunk.len() as u64) {
                    drop(file);
                    let _ = std::fs::remove_file(dest);
                    return Err(err);
                }
                file.write_all(&chunk).map_err(|e| write_failure(&e))?;
                progress.record(chunk.len() as u64);
                total += chunk.len() as u64;
                let _ = written.send(total);
            }
            return Ok(total);
        }
        if let Some(FetchMode::Hold(gate)) = mode {
            gate.notified().await;
        }
        let (failure, bytes) = {
            let mut script = self.script();
            (
                script.fetch_failures.pop_front(),
                script
                    .attachment_bytes
                    .get(&attachment.reference.0)
                    .cloned(),
            )
        };
        if let Some(code) = failure {
            return Err(AdapterError {
                code,
                message: "injected fetch failure".to_owned(),
            });
        }
        let bytes = bytes.unwrap_or_default();
        progress.check_limit(bytes.len() as u64)?;
        create_private_file(dest)?
            .write_all(&bytes)
            .map_err(|e| write_failure(&e))?;
        progress.record(bytes.len() as u64);
        Ok(bytes.len() as u64)
    }

    async fn send_file(
        &self,
        target: &str,
        path: &Path,
        name: &str,
    ) -> Result<SendResult, AdapterError> {
        let data = tokio::fs::read(path).await;
        self.record(Call::SendFile {
            target: target.to_owned(),
            name: name.to_owned(),
            bytes: data.as_ref().map_or(0, |data| data.len() as u64),
        });
        let (failure, hold) = {
            let mut script = self.script();
            (
                script.send_failures.pop_front(),
                script.send_holds.pop_front(),
            )
        };
        if let Some(code) = failure {
            return Err(AdapterError {
                code,
                message: "injected send_file failure".to_owned(),
            });
        }
        if data.is_err() {
            return Err(AdapterError {
                code: E_ADAPTER_UPLOAD_DENIED,
                message: "the file could not be read".to_owned(),
            });
        }
        if let Some(gate) = hold {
            gate.notified().await;
        }
        Ok(SendResult {
            message_id: format!("fake-file-{name}"),
        })
    }

    async fn edit(&self, channel: &str, message_id: &str, text: &str) -> Result<(), AdapterError> {
        self.record(Call::Edit {
            channel: channel.to_owned(),
            message_id: message_id.to_owned(),
            text: text.to_owned(),
        });
        if self.fail_next_edit.swap(false, Ordering::SeqCst) {
            return Err(AdapterError {
                code: E_ADAPTER_EDIT,
                message: "injected edit failure".to_owned(),
            });
        }
        Ok(())
    }

    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        Some(self.reconnects.subscribe())
    }
}

/// A failed local write, like `files::fetch_to_file` reports it: the code and the error kind
/// only, never a path.
fn write_failure(err: &std::io::Error) -> AdapterError {
    AdapterError {
        code: E_FILE_WRITE,
        message: format!("could not write the file: {:?}", err.kind()),
    }
}

/// Creates `dest` like `files::fetch_to_file` does: new, 0600, parent must exist.
fn create_private_file(dest: &Path) -> Result<std::fs::File, AdapterError> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(|e| write_failure(&e))
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
    /// Every `chat.update` body, failed and rate-limited ones included.
    pub updates: Vec<Value>,
    /// Every text the adapter wrote on any socket (the acks).
    pub socket_texts: Vec<String>,
    pub pings: usize,
    pub closed_sockets: usize,
    /// Every file call (downloads and the three upload steps), failed ones included.
    pub file_calls: Vec<SlackFileCall>,
}

/// One file call seen by `FakeSlackTransport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackFileCall {
    Download {
        url_host: String,
        dest: PathBuf,
        reject_html: bool,
    },
    UploadUrl {
        filename: String,
        length: u64,
    },
    UploadBytes {
        host: String,
        len: u64,
    },
    CompleteUpload {
        file_id: String,
        channel_id: String,
        thread_ts: Option<String>,
    },
}

/// A Web API step of the upload flow that `fail_next_upload_step` can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStep {
    UploadUrl,
    CompleteUpload,
}

/// The upload URL and file id the fake hands out unless `set_upload_url` changes the URL.
pub const FAKE_UPLOAD_URL: &str = "https://files.slack.com/upload/v1/fake-111";
pub const FAKE_FILE_ID: &str = "F111";

type SocketItem = Result<SocketMessage, AdapterError>;

#[derive(Default)]
struct FakeSlackState {
    record: SlackRecord,
    fail_auth: bool,
    rate_limits: VecDeque<Duration>,
    fail_next_post: bool,
    fail_next_update: Option<String>,
    rate_limit_next_update: bool,
    sessions: Vec<mpsc::UnboundedSender<SocketItem>>,
    download_bytes: HashMap<String, Vec<u8>>,
    step_failures: VecDeque<(UploadStep, String)>,
    bytes_status_failures: VecDeque<u16>,
    upload_url: Option<String>,
}

impl FakeSlackState {
    /// Pops a scripted Slack error for `step`, classified like the real transport does.
    fn step_failure(&mut self, step: UploadStep) -> Result<(), AdapterError> {
        let Some(index) = self.step_failures.iter().position(|(s, _)| *s == step) else {
            return Ok(());
        };
        let (_, slack_error) = self.step_failures.remove(index).expect("index exists");
        Err(AdapterError {
            code: upload_error_code(&slack_error),
            message: slack_error,
        })
    }
}

fn url_host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
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

    /// The next `update_message` records itself, then fails with a Slack API error
    /// carrying `slack_error`.
    pub fn fail_next_update(&self, slack_error: &str) {
        self.shared
            .update(|state| state.fail_next_update = Some(slack_error.to_owned()));
    }

    /// The next `update_message` records itself, then answers HTTP 429 with a zero
    /// Retry-After.
    pub fn rate_limit_next_update(&self) {
        self.shared
            .update(|state| state.rate_limit_next_update = true);
    }

    /// The bytes `download` writes for this URL (empty when unset).
    pub fn set_download_bytes(&self, url: &str, bytes: Vec<u8>) {
        self.shared.update(|state| {
            state.download_bytes.insert(url.to_owned(), bytes);
        });
    }

    /// The next call of `step` records itself, then answers `ok:false` with `slack_error`.
    pub fn fail_next_upload_step(&self, step: UploadStep, slack_error: &str) {
        self.shared.update(|state| {
            state
                .step_failures
                .push_back((step, slack_error.to_owned()))
        });
    }

    /// The next `upload_bytes` records itself, then fails as the real transport does for
    /// this HTTP status.
    pub fn fail_next_upload_bytes_status(&self, status: u16) {
        self.shared
            .update(|state| state.bytes_status_failures.push_back(status));
    }

    /// The upload URL later `upload_url` calls return (default `FAKE_UPLOAD_URL`).
    pub fn set_upload_url(&self, url: &str) {
        self.shared
            .update(|state| state.upload_url = Some(url.to_owned()));
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

    async fn update_message(&self, body: Value) -> Result<PostOutcome, AdapterError> {
        self.shared.update(|state| {
            let ts = body
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            state.record.updates.push(body);
            if std::mem::take(&mut state.rate_limit_next_update) {
                return Ok(PostOutcome::RateLimited(Duration::ZERO));
            }
            if let Some(slack_error) = state.fail_next_update.take() {
                return Err(AdapterError {
                    code: E_SLACK_API,
                    message: slack_error,
                });
            }
            Ok(PostOutcome::Posted(ts))
        })
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        reject_html: bool,
        progress: &Progress,
    ) -> Result<u64, AdapterError> {
        let bytes = self.shared.update(|state| {
            state.record.file_calls.push(SlackFileCall::Download {
                url_host: url_host(url),
                dest: dest.to_owned(),
                reject_html,
            });
            state.download_bytes.get(url).cloned().unwrap_or_default()
        });
        create_private_file(dest)?
            .write_all(&bytes)
            .map_err(|e| write_failure(&e))?;
        progress.record(bytes.len() as u64);
        Ok(bytes.len() as u64)
    }

    async fn upload_url(
        &self,
        filename: &str,
        length: u64,
    ) -> Result<(String, String), AdapterError> {
        self.shared.update(|state| {
            state.record.file_calls.push(SlackFileCall::UploadUrl {
                filename: filename.to_owned(),
                length,
            });
            state.step_failure(UploadStep::UploadUrl)?;
            let url = state
                .upload_url
                .clone()
                .unwrap_or_else(|| FAKE_UPLOAD_URL.to_owned());
            Ok((url, FAKE_FILE_ID.to_owned()))
        })
    }

    async fn upload_bytes(&self, url: reqwest::Url, bytes: Vec<u8>) -> Result<(), AdapterError> {
        self.shared.update(|state| {
            state.record.file_calls.push(SlackFileCall::UploadBytes {
                host: url.host_str().unwrap_or_default().to_owned(),
                len: bytes.len() as u64,
            });
            match state.bytes_status_failures.pop_front() {
                Some(status) => Err(AdapterError {
                    code: upload_status_code(status),
                    message: format!("the file upload answered HTTP {status}"),
                }),
                None => Ok(()),
            }
        })
    }

    async fn complete_upload(
        &self,
        file_id: &str,
        _title: &str,
        channel_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), AdapterError> {
        self.shared.update(|state| {
            state.record.file_calls.push(SlackFileCall::CompleteUpload {
                file_id: file_id.to_owned(),
                channel_id: channel_id.to_owned(),
                thread_ts: thread_ts.map(str::to_owned),
            });
            state.step_failure(UploadStep::CompleteUpload)
        })
    }
}

/// The appended system prompt a fake-omp child read at start, from its log line
/// `START pid=<pid> prompt=<json> argv=<json>`: the contents of the file its
/// `--append-system-prompt` argument named when it started (a later start may have rewritten
/// the file since), empty when it had no such argument.
pub fn appended(start_line: &str) -> String {
    let (_, rest) = start_line
        .split_once(" prompt=")
        .expect("prompt field in START line");
    let (prompt, _) = rest.split_once(" argv=").expect("argv field in START line");
    serde_json::from_str::<Option<String>>(prompt)
        .expect("prompt JSON")
        .unwrap_or_default()
}

/// Starts a loopback HTTP server that accepts every connection and never answers; returns
/// its `http://` base URL. The server lives as long as the runtime.
pub async fn silent_http_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    format!("http://{addr}/")
}

/// One request the fake jev server received.
#[derive(Debug, Clone)]
pub struct JevRequest {
    pub path: String,
    pub authorization: Option<String>,
    /// The body as sent, so tests can check key order.
    pub raw: String,
    /// The body as JSON, `Null` when it does not parse.
    pub body: Value,
}

enum JevScript {
    Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Value,
    },
    Drop,
}

#[derive(Default)]
struct FakeJevState {
    script: VecDeque<JevScript>,
    requests: Vec<JevRequest>,
}

/// A loopback jev HTTP server. Replies are queued; without one it answers 200 with every
/// noul 0.0 and every choice "none" where offered. Every reply closes its connection.
pub struct FakeJev {
    base_url: String,
    state: Arc<Mutex<FakeJevState>>,
    seen: watch::Receiver<usize>,
}

impl FakeJev {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(FakeJevState::default()));
        let (seen_tx, seen) = watch::channel(0);
        let server_state = state.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                serve_jev(stream, &server_state, &seen_tx).await;
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            state,
            seen,
        }
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    pub fn reply(&self, status: u16, headers: &[(&str, &str)], body: Value) {
        let headers = headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.push(JevScript::Reply {
            status,
            headers,
            body,
        });
    }

    /// Queues a 200 with these answers.
    pub fn answer(&self, answers: Value) {
        self.reply(200, &[], jev_reply_body(answers));
    }

    /// The next connection is closed without an answer.
    pub fn drop_next(&self) {
        self.push(JevScript::Drop);
    }

    pub fn requests(&self) -> Vec<JevRequest> {
        self.state.lock().expect("fake jev lock").requests.clone()
    }

    /// Waits until at least `n` requests arrived; panics after 30 s.
    pub async fn wait_requests(&self, n: usize) {
        let mut seen = self.seen.clone();
        tokio::time::timeout(Duration::from_secs(30), seen.wait_for(|count| *count >= n))
            .await
            .expect("fake jev requests in time")
            .expect("fake jev server alive");
    }

    fn push(&self, script: JevScript) {
        self.state
            .lock()
            .expect("fake jev lock")
            .script
            .push_back(script);
    }
}

fn jev_reply_body(answers: Value) -> Value {
    serde_json::json!({
        "model": "jev-1.14.0",
        "answers": answers,
        "usage": {"input_tokens": 10, "output_tokens": 0},
    })
}

/// Every noul 0.0 and every choice "none" (the first option when "none" is not offered) for
/// the questions of `body`.
fn jev_default_answers(body: &Value) -> Value {
    let answers = body
        .get("questions")
        .and_then(Value::as_object)
        .map(|questions| {
            questions
                .iter()
                .map(|(name, question)| {
                    let answer = match question.get("type").and_then(Value::as_str) {
                        Some("noul") => serde_json::json!({"type": "noul", "noul": 0.0}),
                        _ => {
                            let options = question.get("criteria").and_then(Value::as_object);
                            let choice = match options {
                                Some(options) if !options.contains_key("none") => {
                                    options.keys().next().cloned().unwrap_or_default()
                                }
                                _ => "none".to_owned(),
                            };
                            serde_json::json!({"type": "choice", "choice": choice})
                        }
                    };
                    (name.clone(), answer)
                })
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    Value::Object(answers)
}

async fn serve_jev(
    stream: tokio::net::TcpStream,
    state: &Mutex<FakeJevState>,
    seen: &watch::Sender<usize>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let path = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let mut authorization = None;
    let mut length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_owned());
        }
    }
    let mut raw = vec![0; length];
    if reader.read_exact(&mut raw).await.is_err() {
        return;
    }
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let body = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let script = {
        let mut state = state.lock().expect("fake jev lock");
        let default_answers = jev_default_answers(&body);
        state.requests.push(JevRequest {
            path,
            authorization,
            raw,
            body,
        });
        seen.send_replace(state.requests.len());
        state.script.pop_front().unwrap_or(JevScript::Reply {
            status: 200,
            headers: Vec::new(),
            body: jev_reply_body(default_answers),
        })
    };
    let JevScript::Reply {
        status,
        headers,
        body,
    } = script
    else {
        return;
    };
    let payload = body.to_string();
    let mut head = format!(
        "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        payload.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut stream = reader.into_inner();
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(payload.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AttachmentRef;
    use crate::adapter::E_ADAPTER_FILES;
    use std::os::unix::fs::PermissionsExt;

    fn attachment(reference: &str) -> InboundAttachment {
        InboundAttachment {
            name: "a.bin".into(),
            size: 0,
            mime: None,
            reference: AttachmentRef(reference.into()),
        }
    }

    #[tokio::test]
    async fn fake_open_dm_records_and_can_fail() {
        let fake = FakeAdapter::new();
        assert_eq!(fake.open_dm("111").await, Ok("dm-111".to_owned()));
        fake.fail_next_open_dm();
        assert_eq!(fake.open_dm("111").await.unwrap_err().code, E_ADAPTER_DM);
        assert_eq!(fake.open_dm("111").await, Ok("dm-111".to_owned()));
        let open = Call::OpenDm {
            user_id: "111".into(),
        };
        assert_eq!(fake.calls(), vec![open.clone(), open.clone(), open]);
    }

    #[tokio::test]
    async fn fake_adapter_fetch_records_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");
        let fake = FakeAdapter::new();
        fake.set_attachment_bytes("ref-1", vec![7; 5]);
        let progress = Progress::new();
        let n = fake
            .fetch_attachment(&attachment("ref-1"), &dest, &progress)
            .await
            .unwrap();
        assert_eq!(n, 5);
        assert_eq!(progress.bytes(), 5);
        assert_eq!(std::fs::read(&dest).unwrap(), vec![7; 5]);
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fake.calls(),
            vec![Call::FetchAttachment {
                reference: "ref-1".into(),
                dest
            }]
        );
    }

    #[tokio::test]
    async fn fake_adapter_streamed_fetch_writes_each_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("s.bin");
        let fake = Arc::new(FakeAdapter::new());
        let mut feed = fake.stream_next_fetch();
        let task = {
            let fake = fake.clone();
            let dest = dest.clone();
            tokio::spawn(async move {
                let progress = Progress::new();
                fake.fetch_attachment(&attachment("s"), &dest, &progress)
                    .await
            })
        };
        let limit = Duration::from_secs(30);
        feed.chunks.send(b"abc".to_vec()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(limit, feed.written.recv())
                .await
                .unwrap(),
            Some(3)
        );
        feed.chunks.send(b"def".to_vec()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(limit, feed.written.recv())
                .await
                .unwrap(),
            Some(6)
        );
        drop(feed.chunks);
        let result = tokio::time::timeout(limit, task).await.unwrap().unwrap();
        assert_eq!(result.unwrap(), 6);
        assert_eq!(std::fs::read(&dest).unwrap(), b"abcdef");
    }

    #[tokio::test]
    async fn fake_adapter_held_fetch_waits_for_notify() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("h.bin");
        let fake = Arc::new(FakeAdapter::new());
        fake.set_attachment_bytes("h", vec![1, 2]);
        let gate = fake.hold_next_fetch();
        let task = {
            let fake = fake.clone();
            let dest = dest.clone();
            tokio::spawn(async move {
                fake.fetch_attachment(&attachment("h"), &dest, &Progress::new())
                    .await
            })
        };
        let limit = Duration::from_secs(30);
        tokio::time::timeout(
            limit,
            fake.wait_for_call(|c| matches!(c, Call::FetchAttachment { .. })),
        )
        .await
        .unwrap();
        assert!(
            !dest.exists(),
            "a held fetch writes nothing before the notify"
        );
        assert!(!task.is_finished());
        gate.notify_one();
        let n = tokio::time::timeout(limit, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(n, 2);
        assert!(dest.exists());
    }

    #[tokio::test]
    async fn fake_adapter_fetch_without_parent_dir_is_write_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        let fake = FakeAdapter::new();
        let err = fake
            .fetch_attachment(&attachment("x"), &missing.join("f.bin"), &Progress::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, E_FILE_WRITE);
        assert!(!missing.exists(), "the fake never creates directories");
    }

    #[tokio::test]
    async fn fake_adapter_send_file_unreadable_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.bin");
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let fake = FakeAdapter::new();
        let err = fake.send_file("t", &path, "secret.bin").await.unwrap_err();
        assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);
    }

    #[tokio::test]
    async fn fake_adapter_held_send_file_waits_for_notify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"xyz").unwrap();
        let fake = Arc::new(FakeAdapter::new());
        let gate = fake.hold_next_send_file();
        let task = {
            let fake = fake.clone();
            let path = path.clone();
            tokio::spawn(async move { fake.send_file("t", &path, "f.bin").await })
        };
        let limit = Duration::from_secs(30);
        tokio::time::timeout(
            limit,
            fake.wait_for_call(|c| matches!(c, Call::SendFile { .. })),
        )
        .await
        .unwrap();
        assert!(!task.is_finished());
        gate.notify_one();
        tokio::time::timeout(limit, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn fake_adapter_send_file_reads_whole_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![9u8; 200 * 1024]).unwrap();
        let fake = FakeAdapter::new();
        fake.send_file("thread-1", &path, "big.bin").await.unwrap();
        assert_eq!(
            fake.calls(),
            vec![Call::SendFile {
                target: "thread-1".into(),
                name: "big.bin".into(),
                bytes: 200 * 1024
            }]
        );
    }

    #[tokio::test]
    async fn fake_adapter_injected_file_failures_return_their_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"x").unwrap();
        let fake = FakeAdapter::new();
        fake.fail_next_fetch(E_ADAPTER_FILES);
        fake.fail_next_send_file(E_ADAPTER_FILES);
        let fetch = fake
            .fetch_attachment(&attachment("x"), &dir.path().join("o"), &Progress::new())
            .await
            .unwrap_err();
        assert_eq!(fetch.code, E_ADAPTER_FILES);
        let send = fake.send_file("t", &path, "f.bin").await.unwrap_err();
        assert_eq!(send.code, E_ADAPTER_FILES);
        assert_eq!(fake.calls().len(), 2, "failing calls are recorded");
    }
}
