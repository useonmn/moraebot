//! The core's end of the bridge link: [`BridgeAdapter`], a [`MessengerAdapter`] that
//! talks to a bridge process over a unix socket (`docs/bridge.md`).
//!
//! One connect loop owns the link. It connects through a [`Connector`] (a real unix
//! socket with the peer uid check, or an in-memory stream in tests), reads the bridge's
//! `hello`, answers `hello_ok` and then serves the link until it drops, after which it
//! reconnects with a doubling backoff. Each `hello_ok` bumps [`MessengerAdapter::reconnects`],
//! so replies that failed while the link was down are sent again.
//!
//! The link is multiplexed: every request carries a fresh `id`, and its `ok`, `err` and
//! `progress` frames are matched by `re`, never by line order. A reader task and a writer
//! task keep the socket moving on their own, and inbound messages go into an internal
//! queue, so a request waiting for its `ok` never blocks inbound delivery (and a slow
//! dispatcher never stops the reader from seeing that `ok`). The window bounds the
//! queue: the bridge keeps at most [`WINDOW`] messages unacked per connection, and the
//! core closes a connection whose bridge sends more.
//!
//! Message requests (`send`, `edit`, `thread`, `dm`) wait at most [`REQUEST_DEADLINE`]
//! for their answer, so a bridge that keeps answering pings but never answers one
//! request cannot hang the dispatcher. `status` is not waited for at all.
//!
//! Frames are never logged; logs carry `t`, `id`, `re` and `seq` only.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::peer::PeerError;
use super::proto::{
    Caps, Decoded, Frame, FrameReader, ProtoError, Redacted, RequestKind, StatusState, ThreadsCap,
    WireAttachment, map_error, write_frame,
};
use super::{
    E_BRIDGE_BAD_REQUEST, E_BRIDGE_BUSY, E_BRIDGE_DOWN, E_BRIDGE_PEER, E_BRIDGE_PLATFORM,
    E_BRIDGE_PROTO, E_BRIDGE_UNSUPPORTED, PROTO_VERSION, check_peer,
};
use crate::adapter::{
    AdapterError, AttachmentRef, E_ADAPTER_DM, E_ADAPTER_EDIT, E_ADAPTER_FILES, E_ADAPTER_START,
    E_ADAPTER_THREAD, InboundAttachment, InboundEvent, MessengerAdapter, SendResult, TypingHandle,
    next_backoff,
};
use crate::config::Config;
use crate::files::Progress;

/// File name of the cached last hello inside the moraebot home.
pub const HELLO_CACHE: &str = "bridge-hello.json";
/// The most inbound messages the bridge may keep unacked on one connection.
pub const WINDOW: u32 = 32;
/// How long `open` waits for the first hello before it starts from the cached one.
pub const FIRST_HELLO_WAIT: Duration = Duration::from_secs(10);
/// The core pings the bridge this often.
pub const PING_EVERY: Duration = Duration::from_secs(15);
/// A link with no frame from the bridge for this long is closed.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(45);
/// First reconnect delay; it doubles up to [`BACKOFF_MAX`].
pub const BACKOFF_START: Duration = Duration::from_millis(500);
/// Longest reconnect delay.
pub const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// How long a message request (`send`, `edit`, `thread`, `dm`) waits for its answer.
/// `fetch` and `file` have no deadline here: the dispatcher's stall watchdog and upload
/// time limit bound them.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// The smallest `message_limit` a hello may carry.
pub const MIN_MESSAGE_LIMIT: usize = 100;
/// Capacity of the receiver `start` returns.
const INBOUND_CAPACITY: usize = 64;

/// A connected stream to the bridge.
pub trait Link: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Link for T {}

/// Opens one connection to the bridge. The production connector is [`UnixConnector`];
/// tests hand out in-memory streams.
#[async_trait]
pub trait Connector: Send + Sync + 'static {
    async fn connect(&self) -> io::Result<Box<dyn Link>>;
}

/// Connects to the bridge's unix socket and checks that the bridge runs as this uid.
pub struct UnixConnector {
    path: PathBuf,
}

impl UnixConnector {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl Connector for UnixConnector {
    async fn connect(&self) -> io::Result<Box<dyn Link>> {
        let stream = UnixStream::connect(&self.path).await?;
        check_peer(&stream).map_err(peer_refused)?;
        Ok(Box::new(stream))
    }
}

/// How long `check-config` waits for a bridge to connect and say its first frame.
pub const PROBE_WAIT: Duration = Duration::from_secs(3);

/// What a bridge socket answered to [`probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The bridge said hello.
    Hello {
        bridge: String,
        version: String,
        platform: String,
    },
    /// The bridge serves another core (`E_BRIDGE_BUSY`), so it is up.
    Busy,
}

/// Connects to the bridge at `path` (with the peer uid check), reads its first frame and
/// closes without a `hello_ok`, so the bridge hands the probe no message. `Err` says why
/// nothing usable answered within `wait`.
pub async fn probe(path: &Path, wait: Duration) -> Result<Probe, String> {
    let attempt = async {
        let link = UnixConnector::new(path.to_path_buf())
            .connect()
            .await
            .map_err(|err| format!("cannot connect: {err}"))?;
        probe_link(link).await
    };
    tokio::time::timeout(wait, attempt)
        .await
        .unwrap_or_else(|_| Err(format!("no hello within {}s", wait.as_secs())))
}

/// [`probe`] on a connected link.
pub async fn probe_link(link: impl Link) -> Result<Probe, String> {
    let mut frames = FrameReader::new(BufReader::new(link));
    match frames.next().await {
        Ok(Some(Decoded::Frame(Frame::Hello {
            bridge,
            version,
            platform,
            ..
        }))) => Ok(Probe::Hello {
            bridge,
            version,
            platform,
        }),
        Ok(Some(Decoded::Frame(Frame::Bye { code, .. }))) if code == E_BRIDGE_BUSY => {
            Ok(Probe::Busy)
        }
        Ok(Some(Decoded::Frame(Frame::Bye { code, message }))) => Err(format!(
            "the bridge refused the connection: {code}: {message}"
        )),
        Ok(Some(other)) => Err(format!(
            "the bridge did not start with hello but with {}",
            kind_of(&other)
        )),
        Ok(None) => Err("the bridge closed the link before hello".to_owned()),
        Err(err) => Err(format!("unreadable hello from the bridge: {err}")),
    }
}

/// The connect error for a peer that failed the uid check; [`is_peer_refusal`] spots it.
fn peer_refused(err: PeerError) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, err)
}

/// Whether a connect error is a failed peer uid check, which is always logged.
fn is_peer_refusal(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|inner| inner.is::<PeerError>())
}

/// What the adapter tells the bridge, and where it keeps the last hello.
#[derive(Debug, Clone)]
pub struct BridgeOptions {
    pub owner_id: String,
    pub allow: Vec<String>,
    pub hello_cache: PathBuf,
    pub first_hello_wait: Duration,
}

impl BridgeOptions {
    /// The options of `config`'s `[bridge]` section.
    pub fn from_config(config: &Config) -> Result<Self, AdapterError> {
        let bridge = config.bridge.as_ref().ok_or_else(|| AdapterError {
            code: E_ADAPTER_START,
            message: "messenger = \"bridge\" needs a [bridge] section".to_owned(),
        })?;
        Ok(Self {
            owner_id: bridge.owner_id.clone(),
            allow: bridge
                .access
                .allow
                .iter()
                .map(|entry| entry.id.clone())
                .collect(),
            hello_cache: config.home.join(HELLO_CACHE),
            first_hello_wait: FIRST_HELLO_WAIT,
        })
    }
}

/// The parts of a hello the adapter keeps.
#[derive(Debug, Clone)]
struct HelloInfo {
    platform: String,
    hints: String,
    home: Option<String>,
    message_limit: usize,
    caps: Caps,
}

impl HelloInfo {
    /// Refuses values the core cannot work with: an empty `platform` would make keys
    /// like `:th:...`, and a tiny `message_limit` would split replies into crumbs.
    fn check(&self) -> Result<(), String> {
        if self.platform.trim().is_empty() {
            return Err("the hello has an empty `platform`".to_owned());
        }
        if self.message_limit < MIN_MESSAGE_LIMIT {
            return Err(format!(
                "the hello's `message_limit` {} is below {MIN_MESSAGE_LIMIT}",
                self.message_limit
            ));
        }
        Ok(())
    }

    fn from_frame(frame: &Frame) -> Option<Self> {
        match frame {
            Frame::Hello {
                platform,
                hints,
                home,
                message_limit,
                caps,
                ..
            } => Some(Self {
                platform: platform.clone(),
                hints: hints.clone(),
                home: home.clone(),
                message_limit: *message_limit,
                caps: caps.clone(),
            }),
            _ => None,
        }
    }
}

/// Values fixed by the hello used at open: they shape conversation keys, the prompt
/// and the home target, so they change only at the next start.
#[derive(Debug)]
struct Fixed {
    platform: String,
    hints: String,
    home: Option<String>,
}

/// The fields of an `ok` answer.
#[derive(Debug, Default)]
struct Answer {
    message_id: Option<String>,
    thread_id: Option<String>,
    target: Option<String>,
    bytes: Option<u64>,
}

struct Pending {
    kind: RequestKind,
    reply: oneshot::Sender<Result<Answer, AdapterError>>,
    progress: Option<mpsc::UnboundedSender<u64>>,
}

#[derive(Default)]
struct Requests {
    closed: bool,
    last_id: u64,
    pending: HashMap<u64, Pending>,
}

impl Requests {
    /// The next request id of this connection: above zero and increasing.
    fn fresh_id(&mut self) -> u64 {
        self.last_id += 1;
        self.last_id
    }
}

/// One served connection: its outbound queue and its requests in flight.
struct Session {
    generation: u64,
    out: mpsc::UnboundedSender<Frame>,
    requests: Mutex<Requests>,
}

impl Session {
    fn requests(&self) -> MutexGuard<'_, Requests> {
        self.requests.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends a request whose answer nobody waits for (the typing stop).
    fn fire(&self, build: impl FnOnce(u64) -> Frame) {
        let mut requests = self.requests();
        if requests.closed {
            return;
        }
        let id = requests.fresh_id();
        let _ = self.out.send(build(id));
    }

    fn answer(&self, re: u64, answer: impl FnOnce(RequestKind) -> Result<Answer, AdapterError>) {
        let pending = self.requests().pending.remove(&re);
        match pending {
            Some(pending) => {
                let _ = pending.reply.send(answer(pending.kind));
            }
            None => tracing::debug!(re, "bridge answered a request nobody waits for"),
        }
    }

    fn progress(&self, re: u64, bytes: u64) {
        let requests = self.requests();
        if let Some(tx) = requests
            .pending
            .get(&re)
            .and_then(|pending| pending.progress.as_ref())
        {
            let _ = tx.send(bytes);
        }
    }

    /// Ends every request in flight (their callers see the link drop) and refuses new ones.
    fn close(&self) {
        let mut requests = self.requests();
        requests.closed = true;
        requests.pending.clear();
    }
}

/// A request in flight. Dropping it before the answer arrived sends `cancel`.
struct InFlight {
    session: Arc<Session>,
    id: u64,
    kind: RequestKind,
    reply: oneshot::Receiver<Result<Answer, AdapterError>>,
    done: bool,
}

impl InFlight {
    async fn wait(mut self) -> Result<Answer, AdapterError> {
        let answer = (&mut self.reply).await;
        self.done = true;
        flatten(self.kind, answer)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let removed = self.session.requests().pending.remove(&self.id).is_some();
        if removed {
            tracing::info!(id = self.id, "bridge request given up, cancel sent");
            let _ = self.session.out.send(Frame::Cancel { id: self.id });
        }
    }
}

fn flatten(
    kind: RequestKind,
    answer: Result<Result<Answer, AdapterError>, oneshot::error::RecvError>,
) -> Result<Answer, AdapterError> {
    answer.unwrap_or_else(|_| {
        Err(link_down(
            kind,
            "the link dropped while the request was in flight",
        ))
    })
}

fn link_down(kind: RequestKind, why: &str) -> AdapterError {
    AdapterError {
        code: kind.default_code(),
        message: format!("{E_BRIDGE_DOWN}: {why}"),
    }
}

fn unsupported(code: &'static str, what: &str) -> AdapterError {
    AdapterError {
        code,
        message: format!("the bridge does not support {what}"),
    }
}

fn missing(kind: RequestKind, field: &str) -> AdapterError {
    AdapterError {
        code: kind.default_code(),
        message: format!("{E_BRIDGE_BAD_REQUEST}: the bridge answered without `{field}`"),
    }
}

struct Live {
    caps: Caps,
    message_limit: usize,
    session: Option<Arc<Session>>,
    /// Inbound message id -> (session generation, seq) still to ack.
    unacked: HashMap<String, Vec<(u64, u64)>>,
}

struct Shared {
    options: BridgeOptions,
    fixed: OnceLock<Fixed>,
    /// Set once a session is serving or open started from the cache.
    ready: watch::Sender<bool>,
    live: Mutex<Live>,
    generation: AtomicU64,
    reconnects: watch::Sender<u64>,
    inbound: Mutex<Option<mpsc::UnboundedSender<InboundEvent>>>,
    stop: watch::Sender<bool>,
}

impl Shared {
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn session(&self) -> Option<Arc<Session>> {
        self.live().session.clone()
    }

    fn bump(&self) {
        self.reconnects.send_modify(|n| *n += 1);
    }

    /// Ends delivery: the receiver `start` returned closes once it is drained.
    fn close_inbound(&self) {
        self.inbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    /// Sends the request `build` makes with a fresh id.
    fn begin(
        &self,
        kind: RequestKind,
        progress: Option<mpsc::UnboundedSender<u64>>,
        build: impl FnOnce(u64) -> Frame,
    ) -> Result<InFlight, AdapterError> {
        let session = self
            .session()
            .ok_or_else(|| link_down(kind, "the bridge is not connected"))?;
        let (reply_tx, reply) = oneshot::channel();
        let id = {
            let mut requests = session.requests();
            if requests.closed {
                return Err(link_down(kind, "the bridge is not connected"));
            }
            let id = requests.fresh_id();
            requests.pending.insert(
                id,
                Pending {
                    kind,
                    reply: reply_tx,
                    progress,
                },
            );
            id
        };
        let frame = build(id);
        tracing::debug!(t = frame.kind(), id, "bridge request");
        let _ = session.out.send(frame);
        Ok(InFlight {
            session,
            id,
            kind,
            reply,
            done: false,
        })
    }

    /// Sends a message request and waits up to [`REQUEST_DEADLINE`] for its answer.
    async fn request(
        &self,
        kind: RequestKind,
        build: impl FnOnce(u64) -> Frame,
    ) -> Result<Answer, AdapterError> {
        let call = self.begin(kind, None, build)?;
        match tokio::time::timeout(REQUEST_DEADLINE, call.wait()).await {
            Ok(answer) => answer,
            // The timed-out future dropped the `InFlight`, which sent `cancel`.
            Err(_) => Err(AdapterError {
                code: kind.default_code(),
                message: format!(
                    "the bridge gave no answer within {}s",
                    REQUEST_DEADLINE.as_secs()
                ),
            }),
        }
    }

    /// Queues an inbound message for the dispatcher. `false` when the bridge overran
    /// the window, a contract violation that closes the connection; the message is left
    /// to the bridge's spool.
    fn deliver(&self, generation: u64, seq: u64, event: InboundEvent) -> bool {
        {
            let mut live = self.live();
            let open = live
                .unacked
                .values()
                .flatten()
                .filter(|(g, _)| *g == generation)
                .count();
            if open >= WINDOW as usize {
                tracing::error!(
                    seq,
                    open,
                    "the bridge sent more unacked messages than the window, closing the link"
                );
                return false;
            }
            live.unacked
                .entry(event.message_id.clone())
                .or_default()
                .push((generation, seq));
        }
        let inbound = self.inbound.lock().unwrap_or_else(|e| e.into_inner());
        match inbound.as_ref() {
            Some(tx) => {
                let _ = tx.send(event);
            }
            None => tracing::debug!(seq, "inbound closed, message left to the bridge spool"),
        }
        true
    }

    fn save_hello(&self, hello: &Frame) {
        if let Err(err) = write_private(&self.options.hello_cache, &hello.encode()) {
            tracing::warn!(error = %err, "could not cache the bridge hello");
        }
    }

    fn load_hello(&self) -> Option<HelloInfo> {
        let path = &self.options.hello_cache;
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
            Err(err) => {
                tracing::warn!(error = %err, "could not read the cached bridge hello");
                return None;
            }
        };
        let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let frame = match super::proto::decode_line(line) {
            Ok(Decoded::Frame(frame @ Frame::Hello { .. })) => frame,
            _ => {
                tracing::warn!("the cached bridge hello is not a hello frame, ignored");
                return None;
            }
        };
        if let Frame::Hello { proto, .. } = &frame
            && *proto != PROTO_VERSION
        {
            tracing::warn!(
                proto,
                "the cached bridge hello speaks another proto, ignored"
            );
            return None;
        }
        let info = HelloInfo::from_frame(&frame)?;
        if let Err(why) = info.check() {
            tracing::warn!(reason = %why, "the cached bridge hello is unusable, ignored");
            return None;
        }
        Some(info)
    }
}

/// Writes `bytes` to `path` with mode 0600 through a temp file and a rename.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Why a connection did not get to serving. Repeats of the same `cause` are logged at
/// debug only, so a version skew does not fill the log every ten seconds.
struct Failure {
    cause: &'static str,
    detail: String,
    /// Logged at error rather than warn the first time.
    error: bool,
}

impl Failure {
    fn warn(cause: &'static str, detail: impl Into<String>) -> Self {
        Self {
            cause,
            detail: detail.into(),
            error: false,
        }
    }

    fn error(cause: &'static str, detail: impl Into<String>) -> Self {
        Self {
            cause,
            detail: detail.into(),
            error: true,
        }
    }

    /// Logs this failure; `last` is the cause of the previous one since the last served
    /// connection.
    fn report(self, last: &mut Option<&'static str>) {
        if *last == Some(self.cause) {
            tracing::debug!(detail = %self.detail, "{}", self.cause);
        } else if self.error {
            tracing::error!(detail = %self.detail, "{}", self.cause);
        } else {
            tracing::warn!(detail = %self.detail, "{}", self.cause);
        }
        *last = Some(self.cause);
    }
}

/// How a session ended.
enum Ended {
    /// The link served (or got as far as the handshake) and dropped; reconnect soon.
    Served,
    /// The handshake failed; reconnect with backoff.
    Failed(Failure),
    /// The adapter stops: shutdown, or the bridge changed platform.
    Stop,
}

/// The bridge side of the core: see the module docs.
pub struct BridgeAdapter {
    shared: Arc<Shared>,
    queue: Mutex<Option<mpsc::UnboundedReceiver<InboundEvent>>>,
    task: Mutex<Option<JoinHandle<()>>>,
    forwarder: Mutex<Option<JoinHandle<()>>>,
}

impl BridgeAdapter {
    /// Connects to the bridge of `config`'s `[bridge]` section (see [`Self::open_with`]).
    pub async fn open(config: &Config) -> Result<Self, AdapterError> {
        let options = BridgeOptions::from_config(config)?;
        let socket = config
            .bridge
            .as_ref()
            .map(|bridge| bridge.socket.clone())
            .unwrap_or_default();
        Self::open_with(options, Arc::new(UnixConnector::new(socket))).await
    }

    /// Starts the connect loop and waits up to `first_hello_wait` for a bridge to say
    /// hello. Without one it starts from the cached hello; with neither it fails with
    /// `E_ADAPTER_START`.
    pub async fn open_with(
        options: BridgeOptions,
        connector: Arc<dyn Connector>,
    ) -> Result<Self, AdapterError> {
        let wait = options.first_hello_wait;
        let (inbound_tx, queue) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            options,
            fixed: OnceLock::new(),
            ready: watch::channel(false).0,
            live: Mutex::new(Live {
                caps: Caps::default(),
                message_limit: 0,
                session: None,
                unacked: HashMap::new(),
            }),
            generation: AtomicU64::new(0),
            reconnects: watch::channel(0).0,
            inbound: Mutex::new(Some(inbound_tx)),
            stop: watch::channel(false).0,
        });
        let mut ready = shared.ready.subscribe();
        let task = tokio::spawn(connect_loop(Arc::clone(&shared), connector));
        let adapter = Self {
            shared: Arc::clone(&shared),
            queue: Mutex::new(Some(queue)),
            task: Mutex::new(Some(task)),
            forwarder: Mutex::new(None),
        };
        let greeted = tokio::time::timeout(wait, ready.wait_for(|ready| *ready)).await;
        if matches!(greeted, Ok(Ok(_))) {
            return Ok(adapter);
        }
        let Some(cached) = shared.load_hello() else {
            adapter.shutdown().await;
            return Err(AdapterError {
                code: E_ADAPTER_START,
                message: format!(
                    "{E_BRIDGE_DOWN}: no bridge answered within {}s and no hello is cached",
                    wait.as_secs()
                ),
            });
        };
        {
            let mut live = shared.live();
            if live.session.is_none() {
                live.caps = cached.caps.clone();
                live.message_limit = cached.message_limit;
            }
        }
        let fixed = shared.fixed.get_or_init(|| Fixed {
            platform: cached.platform.clone(),
            hints: cached.hints.clone(),
            home: cached.home.clone(),
        });
        tracing::warn!(
            platform = %fixed.platform,
            "no bridge answered at start, starting from the cached hello"
        );
        shared.ready.send_replace(true);
        Ok(adapter)
    }

    fn fixed(&self) -> &Fixed {
        self.shared
            .fixed
            .get()
            .expect("open sets the fixed hello values before it returns")
    }

    fn caps(&self) -> Caps {
        self.shared.live().caps.clone()
    }
}

impl Drop for BridgeAdapter {
    fn drop(&mut self) {
        self.shared.stop.send_replace(true);
        for slot in [&self.task, &self.forwarder] {
            if let Some(task) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                task.abort();
            }
        }
    }
}

#[async_trait]
impl MessengerAdapter for BridgeAdapter {
    fn platform(&self) -> &str {
        &self.fixed().platform
    }

    fn message_limit(&self) -> usize {
        self.shared.live().message_limit.max(1)
    }

    fn behavior_hints(&self) -> &str {
        &self.fixed().hints
    }

    fn home_target(&self) -> Option<String> {
        self.fixed().home.clone()
    }

    async fn start(&self) -> Result<mpsc::Receiver<InboundEvent>, AdapterError> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| AdapterError {
                code: E_ADAPTER_START,
                message: "the bridge adapter was started twice".to_owned(),
            })?;
        let (tx, rx) = mpsc::channel(INBOUND_CAPACITY);
        let forwarder = tokio::spawn(async move {
            while let Some(event) = queue.recv().await {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        *self.forwarder.lock().unwrap_or_else(|e| e.into_inner()) = Some(forwarder);
        Ok(rx)
    }

    async fn send(&self, channel: &str, text: &str) -> Result<SendResult, AdapterError> {
        let kind = RequestKind::Send;
        let answer = self
            .shared
            .request(kind, |id| Frame::Send {
                id,
                target: channel.to_owned(),
                text: Redacted(text.to_owned()),
            })
            .await?;
        let message_id = answer
            .message_id
            .ok_or_else(|| missing(kind, "message_id"))?;
        Ok(SendResult { message_id })
    }

    async fn create_thread(
        &self,
        channel_id: &str,
        from_message_id: &str,
        name: &str,
    ) -> Result<String, AdapterError> {
        match self.caps().threads {
            ThreadsCap::Implicit => Ok(from_message_id.to_owned()),
            ThreadsCap::None => Err(unsupported(E_ADAPTER_THREAD, "threads")),
            ThreadsCap::Create => {
                let kind = RequestKind::Thread;
                let answer = self
                    .shared
                    .request(kind, |id| Frame::Thread {
                        id,
                        channel_id: channel_id.to_owned(),
                        from_message_id: from_message_id.to_owned(),
                        name: Redacted(name.to_owned()),
                    })
                    .await?;
                answer.thread_id.ok_or_else(|| missing(kind, "thread_id"))
            }
        }
    }

    async fn start_typing(&self, channel: &str) -> Result<TypingHandle, AdapterError> {
        if !self.caps().status {
            return Ok(TypingHandle::new(|| {}));
        }
        // Fire and forget: the turn must not wait on the messenger's status call, and a
        // failed status is only cosmetic.
        let session = self
            .shared
            .session()
            .ok_or_else(|| link_down(RequestKind::Status, "the bridge is not connected"))?;
        session.fire(|id| Frame::Status {
            id,
            target: channel.to_owned(),
            state: StatusState::Start,
        });
        let shared = Arc::clone(&self.shared);
        let target = channel.to_owned();
        Ok(TypingHandle::new(move || {
            if let Some(session) = shared.session() {
                session.fire(|id| Frame::Status {
                    id,
                    target,
                    state: StatusState::Stop,
                });
            }
        }))
    }

    async fn open_dm(&self, user_id: &str) -> Result<String, AdapterError> {
        if !self.caps().dm {
            return Err(unsupported(E_ADAPTER_DM, "DMs"));
        }
        let kind = RequestKind::Dm;
        let answer = self
            .shared
            .request(kind, |id| Frame::Dm {
                id,
                user_id: user_id.to_owned(),
            })
            .await?;
        answer.target.ok_or_else(|| missing(kind, "target"))
    }

    async fn fetch_attachment(
        &self,
        attachment: &InboundAttachment,
        dest: &Path,
        progress: &Progress,
    ) -> Result<u64, AdapterError> {
        if !self.caps().files {
            return Err(unsupported(E_ADAPTER_FILES, "files"));
        }
        let (progress_tx, mut totals) = mpsc::unbounded_channel();
        let mut call = self
            .shared
            .begin(RequestKind::Fetch, Some(progress_tx), |id| Frame::Fetch {
                id,
                attachment: WireAttachment {
                    name: attachment.name.clone(),
                    size: attachment.size,
                    mime: attachment.mime.clone(),
                    reference: Redacted(attachment.reference.0.clone()),
                },
                dest: dest.to_owned(),
                max_bytes: progress.limit(),
            })?;
        let mut seen = 0;
        loop {
            tokio::select! {
                biased;
                Some(total) = totals.recv() => {
                    // Returning here drops `call`, which cancels the download.
                    progress.check_limit(total)?;
                    if total > seen {
                        progress.record(total - seen);
                        seen = total;
                    }
                }
                answer = &mut call.reply => {
                    call.done = true;
                    let answer = flatten(call.kind, answer)?;
                    // The final total counts like a progress frame: a bridge that
                    // answers past the limit without progress is still refused.
                    let total = answer.bytes.unwrap_or(seen);
                    progress.check_limit(total)?;
                    if total > seen {
                        progress.record(total - seen);
                    }
                    return Ok(total);
                }
            }
        }
    }

    async fn send_file(
        &self,
        target: &str,
        path: &Path,
        name: &str,
    ) -> Result<SendResult, AdapterError> {
        if !self.caps().files {
            return Err(unsupported(E_ADAPTER_FILES, "files"));
        }
        let kind = RequestKind::File;
        // No `REQUEST_DEADLINE` here: the dispatcher's `files::UPLOAD_TIMEOUT` bounds
        // an upload, and dropping this future cancels it.
        let answer = self
            .shared
            .begin(kind, None, |id| Frame::File {
                id,
                target: target.to_owned(),
                path: path.to_owned(),
                name: name.to_owned(),
            })?
            .wait()
            .await?;
        let message_id = answer
            .message_id
            .ok_or_else(|| missing(kind, "message_id"))?;
        Ok(SendResult { message_id })
    }

    async fn edit(&self, target: &str, message_id: &str, text: &str) -> Result<(), AdapterError> {
        if !self.caps().edit {
            return Err(unsupported(E_ADAPTER_EDIT, "editing messages"));
        }
        self.shared
            .request(RequestKind::Edit, |id| Frame::Edit {
                id,
                target: target.to_owned(),
                message_id: message_id.to_owned(),
                text: Redacted(text.to_owned()),
            })
            .await
            .map(|_| ())
    }

    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        Some(self.shared.reconnects.subscribe())
    }

    async fn ack_inbound(&self, message_id: &str) {
        let (session, seqs) = {
            let mut live = self.shared.live();
            let seqs = live.unacked.remove(message_id).unwrap_or_default();
            (live.session.clone(), seqs)
        };
        let Some(session) = session else {
            return;
        };
        for (generation, seq) in seqs {
            // A seq of an earlier connection means nothing now; the bridge sends the
            // message again on the new one.
            if generation == session.generation {
                let _ = session.out.send(Frame::Ack { seq });
            }
        }
    }

    async fn shutdown(&self) {
        self.shared.stop.send_replace(true);
        self.shared.close_inbound();
        let task = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(task) = task
            && let Err(err) = task.await
            && !err.is_cancelled()
        {
            tracing::error!(error = %err, "bridge connect loop panicked");
        }
    }
}

async fn connect_loop(shared: Arc<Shared>, connector: Arc<dyn Connector>) {
    let mut stop = shared.stop.subscribe();
    let mut delay = BACKOFF_START;
    let mut last_failure = None;
    loop {
        let attempt = tokio::select! {
            attempt = connector.connect() => attempt,
            _ = stop.wait_for(|stop| *stop) => return,
        };
        match attempt {
            Ok(link) => match run_session(&shared, link, &mut stop).await {
                Ended::Served => {
                    delay = BACKOFF_START;
                    last_failure = None;
                }
                Ended::Failed(failure) => failure.report(&mut last_failure),
                Ended::Stop => return,
            },
            // The one security-relevant link event: logged every time.
            Err(err) if is_peer_refusal(&err) => tracing::error!(
                code = E_BRIDGE_PEER,
                error = %err,
                "the bridge socket is served by another user, refused"
            ),
            Err(err) => {
                Failure::warn("cannot reach the bridge, retrying", err.to_string())
                    .report(&mut last_failure);
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop.wait_for(|stop| *stop) => return,
        }
        delay = next_backoff(delay, BACKOFF_START, BACKOFF_MAX);
    }
}

type Frames = mpsc::UnboundedReceiver<Result<Decoded, ProtoError>>;

/// Reads frames until the stream ends or breaks, so the session loop never waits on a
/// half-read line.
async fn read_frames(
    read: ReadHalf<Box<dyn Link>>,
    frames: mpsc::UnboundedSender<Result<Decoded, ProtoError>>,
) {
    let mut reader = FrameReader::new(BufReader::new(read));
    loop {
        match reader.next().await {
            Ok(Some(decoded)) => {
                if frames.send(Ok(decoded)).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(err @ (ProtoError::LineTooLong | ProtoError::Io(_))) => {
                let _ = frames.send(Err(err));
                return;
            }
            Err(err) => {
                if frames.send(Err(err)).is_err() {
                    return;
                }
            }
        }
    }
}

async fn write_frames(
    mut write: WriteHalf<Box<dyn Link>>,
    mut out: mpsc::UnboundedReceiver<Frame>,
) {
    while let Some(frame) = out.recv().await {
        if let Err(err) = write_frame(&mut write, &frame).await {
            tracing::warn!(t = frame.kind(), error = %err, "bridge link write failed");
            return;
        }
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn run_session(
    shared: &Arc<Shared>,
    link: Box<dyn Link>,
    stop: &mut watch::Receiver<bool>,
) -> Ended {
    let (read, mut write) = tokio::io::split(link);
    let (frames_tx, mut frames) = mpsc::unbounded_channel();
    let _reader = AbortOnDrop(tokio::spawn(read_frames(read, frames_tx)));

    let first = tokio::select! {
        first = tokio::time::timeout(SILENCE_LIMIT, frames.recv()) => first,
        _ = stop.wait_for(|stop| *stop) => return Ended::Stop,
    };
    let hello = match first {
        Ok(Some(Ok(Decoded::Frame(hello @ Frame::Hello { .. })))) => hello,
        Ok(Some(Ok(Decoded::Frame(Frame::Bye { code, .. })))) => {
            return Ended::Failed(Failure::warn("the bridge refused the connection", code));
        }
        Ok(Some(Ok(other))) => {
            return Ended::Failed(Failure::warn(
                "the bridge did not start with hello",
                kind_of(&other),
            ));
        }
        Ok(Some(Err(err))) => {
            return Ended::Failed(Failure::warn(
                "unreadable hello from the bridge",
                err.to_string(),
            ));
        }
        Ok(None) => {
            return Ended::Failed(Failure::warn("the bridge closed the link before hello", ""));
        }
        Err(_) => {
            return Ended::Failed(Failure::warn("the bridge sent no hello in time", ""));
        }
    };
    let Frame::Hello { proto, .. } = &hello else {
        unreachable!("matched above");
    };
    if *proto != PROTO_VERSION {
        let message = format!("the core speaks proto {PROTO_VERSION}, the bridge sent {proto}");
        let bye = Frame::Bye {
            code: E_BRIDGE_PROTO.to_owned(),
            message: message.clone(),
        };
        let _ = write_frame(&mut write, &bye).await;
        return Ended::Failed(Failure::error(
            "E_BRIDGE_PROTO: the bridge speaks another proto",
            message,
        ));
    }
    let info = HelloInfo::from_frame(&hello).expect("a hello frame");
    if let Err(why) = info.check() {
        let bye = Frame::Bye {
            code: E_BRIDGE_BAD_REQUEST.to_owned(),
            message: why.clone(),
        };
        let _ = write_frame(&mut write, &bye).await;
        return Ended::Failed(Failure::error(
            "E_BRIDGE_BAD_REQUEST: the bridge sent an unusable hello",
            why,
        ));
    }
    let fixed = shared.fixed.get_or_init(|| Fixed {
        platform: info.platform.clone(),
        hints: info.hints.clone(),
        home: info.home.clone(),
    });
    if fixed.platform != info.platform {
        tracing::error!(
            code = E_BRIDGE_PLATFORM,
            started = %fixed.platform,
            now = %info.platform,
            "the bridge changed platform, stopping so the service restarts with it"
        );
        let bye = Frame::Bye {
            code: E_BRIDGE_PLATFORM.to_owned(),
            message: format!(
                "the core started as {}, the bridge now reports {}",
                fixed.platform, info.platform
            ),
        };
        let _ = write_frame(&mut write, &bye).await;
        shared.close_inbound();
        shared.stop.send_replace(true);
        return Ended::Stop;
    }
    if fixed.hints != info.hints || fixed.home != info.home {
        tracing::info!("the bridge changed its hints or home; they apply at the next start");
    }
    shared.save_hello(&hello);
    let hello_ok = Frame::HelloOk {
        proto: PROTO_VERSION,
        core: env!("CARGO_PKG_VERSION").to_owned(),
        owner_id: shared.options.owner_id.clone(),
        allow: shared.options.allow.clone(),
        window: WINDOW,
    };
    if let Err(err) = write_frame(&mut write, &hello_ok).await {
        return Ended::Failed(Failure::warn(
            "could not answer the bridge hello",
            err.to_string(),
        ));
    }

    let (out, out_rx) = mpsc::unbounded_channel();
    let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let session = Arc::new(Session {
        generation,
        out,
        requests: Mutex::new(Requests::default()),
    });
    {
        let mut live = shared.live();
        live.caps = info.caps.clone();
        live.message_limit = info.message_limit;
        live.session = Some(Arc::clone(&session));
        live.unacked.clear();
    }
    let mut writer = tokio::spawn(write_frames(write, out_rx));
    shared.bump();
    shared.ready.send_replace(true);
    if let Frame::Hello {
        bridge, spooled, ..
    } = &hello
    {
        tracing::info!(
            bridge = %bridge,
            platform = %info.platform,
            agent_mode = info.caps.agent_mode,
            spooled,
            "bridge connected"
        );
    }

    let ended = serve_link(shared, &session, &mut frames, &mut writer, stop).await;
    writer.abort();
    {
        let mut live = shared.live();
        if live
            .session
            .as_ref()
            .is_some_and(|current| current.generation == generation)
        {
            live.session = None;
        }
    }
    session.close();
    ended
}

fn kind_of(decoded: &Decoded) -> String {
    match decoded {
        Decoded::Frame(frame) => frame.kind().to_owned(),
        Decoded::Unknown { t, .. } => t.clone(),
    }
}

async fn serve_link(
    shared: &Arc<Shared>,
    session: &Arc<Session>,
    frames: &mut Frames,
    writer: &mut JoinHandle<()>,
    stop: &mut watch::Receiver<bool>,
) -> Ended {
    let mut ping = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let mut pings = 0;
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            _ = stop.wait_for(|stop| *stop) => return Ended::Stop,
            _ = &mut *writer => {
                tracing::warn!("the bridge link stopped taking frames");
                return Ended::Served;
            }
            _ = ping.tick() => {
                pings += 1;
                let _ = session.out.send(Frame::Ping { n: pings });
            }
            _ = tokio::time::sleep_until(last_heard + SILENCE_LIMIT) => {
                tracing::warn!("the bridge was silent for {}s, closing the link", SILENCE_LIMIT.as_secs());
                return Ended::Served;
            }
            next = frames.recv() => {
                last_heard = Instant::now();
                match next {
                    None => {
                        tracing::info!("the bridge closed the link");
                        return Ended::Served;
                    }
                    Some(Ok(decoded)) => {
                        if !handle_frame(shared, session, decoded) {
                            return Ended::Served;
                        }
                    }
                    Some(Err(ProtoError::BadFrame { t, id: Some(id), reason })) => {
                        tracing::warn!(t = %t, id, reason = %reason, "bad frame from the bridge");
                        let _ = session.out.send(Frame::Err {
                            re: id,
                            code: E_BRIDGE_BAD_REQUEST.to_owned(),
                            message: format!("bad `{t}` frame: {reason}"),
                        });
                    }
                    Some(Err(err @ (ProtoError::LineTooLong | ProtoError::Io(_)))) => {
                        tracing::warn!(error = %err, "the bridge link broke");
                        return Ended::Served;
                    }
                    Some(Err(err)) => tracing::warn!(error = %err, "unreadable frame from the bridge, ignored"),
                }
            }
        }
    }
}

/// Handles one frame from the bridge; `false` ends the session.
fn handle_frame(shared: &Shared, session: &Session, decoded: Decoded) -> bool {
    let frame = match decoded {
        Decoded::Frame(frame) => frame,
        Decoded::Unknown { t, id } => {
            tracing::info!(t = %t, ?id, "unknown frame type from the bridge");
            if let Some(id) = id {
                let _ = session.out.send(Frame::Err {
                    re: id,
                    code: E_BRIDGE_UNSUPPORTED.to_owned(),
                    message: format!("the core does not know `{t}` frames"),
                });
            }
            return true;
        }
    };
    match frame {
        Frame::Message {
            seq,
            message_id,
            author_id,
            author_is_bot,
            channel_id,
            thread_id,
            is_dm,
            text,
            attachments,
            redelivery,
        } => {
            tracing::debug!(seq, redelivery, "bridge message");
            let event = InboundEvent {
                message_id,
                author_id,
                author_is_bot,
                channel_id,
                thread_id,
                is_dm,
                text: text.0,
                attachments: attachments
                    .into_iter()
                    .map(|a| InboundAttachment {
                        name: a.name,
                        size: a.size,
                        mime: a.mime,
                        reference: AttachmentRef(a.reference.0),
                    })
                    .collect(),
            };
            if !shared.deliver(session.generation, seq, event) {
                return false;
            }
        }
        Frame::Ok {
            re,
            message_id,
            thread_id,
            target,
            bytes,
        } => session.answer(re, |_| {
            Ok(Answer {
                message_id,
                thread_id,
                target,
                bytes,
            })
        }),
        Frame::Err { re, code, message } => {
            tracing::debug!(re, code = %code, "bridge request failed");
            session.answer(re, |kind| {
                let (code, message) = map_error(kind, &code, &message);
                Err(AdapterError { code, message })
            });
        }
        Frame::Progress { re, bytes } => session.progress(re, bytes),
        Frame::Event { name } if name == "reconnected" => {
            tracing::info!("the bridge's messenger reconnected");
            shared.bump();
        }
        Frame::Event { name } => tracing::debug!(name = %name, "unknown bridge event, ignored"),
        Frame::Ping { n } => {
            let _ = session.out.send(Frame::Pong { n });
        }
        Frame::Pong { .. } => {}
        Frame::Bye { code, .. } => {
            tracing::warn!(code = %code, "the bridge closed the link");
            return false;
        }
        other => {
            tracing::warn!(t = other.kind(), "unexpected frame from the bridge");
            if let Some(id) = other.request_id() {
                let _ = session.out.send(Frame::Err {
                    re: id,
                    code: E_BRIDGE_UNSUPPORTED.to_owned(),
                    message: format!("the core does not serve `{}` requests", other.kind()),
                });
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_peer_check_is_told_apart_from_other_connect_errors() {
        assert!(is_peer_refusal(&peer_refused(PeerError::Mismatch {
            peer: 1001,
            own: 1000
        })));
        assert!(!is_peer_refusal(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(!is_peer_refusal(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket mode"
        )));
    }

    /// The answer of [`probe_link`] to a bridge that writes `lines` and then closes.
    async fn probe_lines(lines: &str) -> Result<Probe, String> {
        let (core, mut bridge) = tokio::io::duplex(4096);
        tokio::io::AsyncWriteExt::write_all(&mut bridge, lines.as_bytes())
            .await
            .expect("write");
        drop(bridge);
        probe_link(core).await
    }

    #[tokio::test]
    async fn a_probe_reads_the_hello_or_a_busy_bye() {
        let hello = r#"{"t":"hello","proto":1,"bridge":"moraebot-slack-bridge","version":"0.9.0","platform":"slack","message_limit":3900,"hints":"h"}"#;
        assert_eq!(
            probe_lines(&format!("{hello}\n")).await,
            Ok(Probe::Hello {
                bridge: "moraebot-slack-bridge".to_owned(),
                version: "0.9.0".to_owned(),
                platform: "slack".to_owned(),
            })
        );
        assert_eq!(
            probe_lines("{\"t\":\"bye\",\"code\":\"E_BRIDGE_BUSY\",\"message\":\"x\"}\n").await,
            Ok(Probe::Busy)
        );
        for (lines, expected) in [
            (
                "{\"t\":\"bye\",\"code\":\"E_BRIDGE_PROTO\",\"message\":\"x\"}\n",
                "E_BRIDGE_PROTO",
            ),
            ("{\"t\":\"ping\",\"n\":1}\n", "ping"),
            ("", "closed"),
            ("not json\n", "unreadable"),
        ] {
            let err = probe_lines(lines).await.expect_err(lines);
            assert!(err.contains(expected), "{lines:?}: {err}");
        }
    }

    #[tokio::test]
    async fn a_probe_of_a_socket_nobody_listens_on_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = probe(&dir.path().join("none.sock"), PROBE_WAIT)
            .await
            .expect_err("nothing listens");
        assert!(err.starts_with("cannot connect"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_gives_up_on_a_silent_bridge() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("silent.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        let accept = tokio::spawn(async move { listener.accept().await });
        let err = probe(&path, PROBE_WAIT).await.expect_err("silent");
        assert_eq!(err, "no hello within 3s");
        drop(accept);
    }

    #[test]
    fn a_hello_needs_a_platform_and_a_usable_message_limit() {
        let info = |platform: &str, message_limit| HelloInfo {
            platform: platform.to_owned(),
            hints: String::new(),
            home: None,
            message_limit,
            caps: Caps::default(),
        };
        assert!(info("slack", MIN_MESSAGE_LIMIT).check().is_ok());
        assert!(info("", 3900).check().is_err());
        assert!(info(" ", 3900).check().is_err());
        assert!(info("slack", 0).check().is_err());
        assert!(info("slack", MIN_MESSAGE_LIMIT - 1).check().is_err());
    }
}
