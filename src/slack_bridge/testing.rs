//! Offline Slack fakes for the bridge tests: `FakeSlackTransport` records every Web API
//! call and drives scripted Socket Mode sessions, and [`TestBridge`] runs a whole bridge
//! on it in process. Nothing reaches the network.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::adapter::AdapterError;
use crate::bridge::client::{BridgeAdapter, BridgeOptions, HELLO_CACHE, UnixConnector};
use crate::bridge::peer::own_uid;
use crate::bridge::socket::SOCKET_NAME;
use crate::files::Progress;
use crate::slack_bridge::slack::{
    E_SLACK_API, E_SLACK_AUTH, PostOutcome, SlackSocket, SlackTransport, SocketMessage,
    upload_error_code, upload_status_code,
};
use crate::slack_bridge::{Bridge, BridgeError, BridgeParts, STATE_DIR};
use crate::testing::{create_private_file, write_failure};

/// The channel a [`TestBridge`] reports as `home`.
pub const TEST_CHANNEL: &str = "C222";

/// A bridge on a [`FakeSlackTransport`], with its socket and state under one folder.
pub struct TestBridge {
    pub fake: Arc<FakeSlackTransport>,
    pub dir: PathBuf,
    bridge: Bridge,
}

impl TestBridge {
    /// Starts a bridge in `dir` that accepts cores of this uid.
    pub async fn start(dir: &Path, fake: Arc<FakeSlackTransport>) -> Self {
        Self::start_with(dir, fake, own_uid())
            .await
            .expect("the test bridge starts")
    }

    /// Starts a bridge in `dir` that accepts only cores running as `expected_uid`.
    pub async fn start_with(
        dir: &Path,
        fake: Arc<FakeSlackTransport>,
        expected_uid: u32,
    ) -> Result<Self, BridgeError> {
        let bridge = Bridge::start(BridgeParts {
            state_dir: dir.join(STATE_DIR),
            socket: Self::socket_in(dir),
            channel_id: TEST_CHANNEL.to_owned(),
            transport: fake.clone(),
            expected_uid,
        })
        .await?;
        Ok(Self {
            fake,
            dir: dir.to_owned(),
            bridge,
        })
    }

    /// The socket path a bridge started in `dir` listens on.
    pub fn socket_in(dir: &Path) -> PathBuf {
        dir.join("run").join(SOCKET_NAME)
    }

    pub fn socket(&self) -> &Path {
        self.bridge.socket()
    }

    pub fn spool_dir(&self) -> PathBuf {
        self.dir.join(STATE_DIR).join("spool")
    }

    /// A core-side [`BridgeAdapter`] connected to this bridge with `owner_id` and `allow`.
    pub async fn connect(&self, owner_id: &str, allow: &[&str]) -> BridgeAdapter {
        let options = BridgeOptions {
            owner_id: owner_id.to_owned(),
            allow: allow.iter().map(|id| (*id).to_owned()).collect(),
            hello_cache: self.dir.join(HELLO_CACHE),
            first_hello_wait: Duration::from_secs(10),
        };
        BridgeAdapter::open_with(
            options,
            Arc::new(UnixConnector::new(self.socket().to_owned())),
        )
        .await
        .expect("the core connects to the test bridge")
    }

    pub async fn shutdown(self) {
        self.bridge.shutdown().await;
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
    /// Every `chat.update` body, failed and rate-limited ones included.
    pub updates: Vec<Value>,
    /// Every text the adapter wrote on any socket (the acks).
    pub socket_texts: Vec<String>,
    pub pings: usize,
    pub closed_sockets: usize,
    /// Every file call (downloads and the three upload steps), failed ones included.
    pub file_calls: Vec<SlackFileCall>,
    /// Every `assistant.threads.setStatus` call, failed ones included.
    pub statuses: Vec<StatusCall>,
}

/// One `assistant.threads.setStatus` call seen by `FakeSlackTransport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCall {
    pub channel_id: String,
    pub thread_ts: String,
    /// Empty when the call cleared the status.
    pub status: String,
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
    status_failures: VecDeque<String>,
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

    /// The next `set_status` records itself, then answers `ok:false` with `slack_error`.
    /// Calls queue up.
    pub fn fail_next_status(&self, slack_error: &str) {
        self.shared
            .update(|state| state.status_failures.push_back(slack_error.to_owned()));
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
        progress.check_limit(bytes.len() as u64)?;
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

    async fn set_status(
        &self,
        channel_id: &str,
        thread_ts: &str,
        status: &str,
    ) -> Result<(), AdapterError> {
        self.shared.update(|state| {
            state.record.statuses.push(StatusCall {
                channel_id: channel_id.to_owned(),
                thread_ts: thread_ts.to_owned(),
                status: status.to_owned(),
            });
            match state.status_failures.pop_front() {
                Some(slack_error) => Err(AdapterError {
                    code: E_SLACK_API,
                    message: slack_error,
                }),
                None => Ok(()),
            }
        })
    }
}
