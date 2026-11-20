//! The bridge side of the link (`docs/bridge.md`): the socket the core connects to, one
//! core session at a time, the spool handover and the core's requests.
//!
//! A session says `hello`, waits for `hello_ok`, then hands the spool over in `seq`
//! order with at most `window` messages unacked. Each spooled payload is mapped with the
//! scope of this connection (the core's owner and allow list, the configured channel,
//! the bot user id), so a message the core would not take is deleted here and never
//! sent. Every request runs on its own task and is answered by its `id`, so a download
//! never holds a send. A `status` is not a task: the session keeps one Slack status per
//! DM thread target until the matching `stop`. When the session ends, its requests are
//! stopped, their part files removed and its statuses cleared; unacked messages stay in
//! the spool for the next core.
//!
//! Frames are never logged; logs carry `t`, `id`, `re` and `seq` only.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{BufReader, ReadHalf, WriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;

use super::{BRIDGE_NAME, BridgeError, E_BRIDGE_STOPPING};
use crate::adapter::{AdapterError, AttachmentRef, InboundAttachment, MessengerAdapter};
use crate::bridge::proto::{
    Caps, Decoded, Frame, FrameReader, MAX_LINE, ProtoError, Redacted, RequestKind, StatusState,
    ThreadsCap, WireAttachment, map_error, write_frame,
};
use crate::bridge::spool::{Attempt, Spool};
use crate::bridge::{
    E_BRIDGE_BAD_REQUEST, E_BRIDGE_BUSY, E_BRIDGE_PEER, E_BRIDGE_PROTO, E_BRIDGE_SOCKET,
    E_BRIDGE_UNSUPPORTED, PROTO_VERSION, peer_uid, socket,
};
use crate::files::{self, Progress};
use crate::slack_bridge::slack::{
    DEFAULT_HINTS, MESSAGE_LIMIT, PLATFORM, Scope, SlackLink, SpoolFilter, StatusKeeper, map_event,
    unix_now,
};

/// A link with no frame from the core for this long is closed (the core pings every 15 s).
pub const SILENCE_LIMIT: Duration = Duration::from_secs(45);
/// How often a download reports its running total while bytes flow.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(1);
/// The size cap of a download the core sent without `max_bytes`.
pub const FETCH_CAP: u64 = 1 << 30;
/// How long shutdown waits for the session to say `bye` and close.
const SESSION_STOP_WAIT: Duration = Duration::from_secs(5);

/// What the server needs besides the Slack link.
pub struct ServerOptions {
    pub socket: PathBuf,
    /// The channel the bridge reports as `home` and maps channel messages from.
    pub channel_id: String,
    /// The uid a core must run as; production passes this process's own uid.
    pub expected_uid: u32,
}

struct Ctx {
    link: Arc<SlackLink>,
    spool: Arc<Spool>,
    /// Learns each core's owner and allow list, so the Slack side spools only what a
    /// core would take.
    filter: Arc<SpoolFilter>,
    channel_id: String,
    appended: watch::Receiver<u64>,
    bot_user_id: watch::Receiver<Option<String>>,
}

/// The listening socket and its accept loop.
pub struct Server {
    socket: PathBuf,
    stop: watch::Sender<bool>,
    accept: JoinHandle<()>,
}

impl Server {
    /// Makes the socket folder private, replaces a stale socket file, binds with mode
    /// 0600 and starts accepting cores.
    pub fn bind(
        options: ServerOptions,
        link: Arc<SlackLink>,
        spool: Arc<Spool>,
        filter: Arc<SpoolFilter>,
        appended: watch::Receiver<u64>,
        bot_user_id: watch::Receiver<Option<String>>,
    ) -> Result<Self, BridgeError> {
        socket::prepare_dir(&options.socket).map_err(BridgeError::from_config)?;
        remove_stale_socket(&options.socket)?;
        let listener = UnixListener::bind(&options.socket).map_err(|err| {
            BridgeError::new(
                E_BRIDGE_SOCKET,
                format!("cannot listen on {}: {err}", options.socket.display()),
            )
        })?;
        fs::set_permissions(&options.socket, fs::Permissions::from_mode(0o600)).map_err(|err| {
            BridgeError::new(
                E_BRIDGE_SOCKET,
                format!("cannot chmod 0600 {}: {err}", options.socket.display()),
            )
        })?;
        let ctx = Arc::new(Ctx {
            link,
            spool,
            filter,
            channel_id: options.channel_id,
            appended,
            bot_user_id,
        });
        let stop = watch::channel(false).0;
        let accept = tokio::spawn(accept_loop(
            listener,
            ctx,
            options.expected_uid,
            stop.subscribe(),
        ));
        tracing::info!(socket = %options.socket.display(), "bridge socket listening");
        Ok(Self {
            socket: options.socket,
            stop,
            accept,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Says `bye` to a connected core, stops accepting and removes the socket file.
    pub async fn shutdown(self) {
        self.stop.send_replace(true);
        if let Err(err) = self.accept.await
            && !err.is_cancelled()
        {
            tracing::error!(error = %err, "bridge accept loop panicked");
        }
        match fs::remove_file(&self.socket) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(error = %err, "could not remove the bridge socket"),
        }
    }
}

/// Removes a socket file a stopped bridge left behind. Anything that is not a socket is
/// refused: the path is wrong, and deleting it would be a surprise.
fn remove_stale_socket(path: &Path) -> Result<(), BridgeError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path).map_err(|err| {
            BridgeError::new(
                E_BRIDGE_SOCKET,
                format!("cannot remove the stale socket {}: {err}", path.display()),
            )
        }),
        Ok(_) => Err(BridgeError::new(
            E_BRIDGE_SOCKET,
            format!("{} exists and is not a socket", path.display()),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(BridgeError::new(
            E_BRIDGE_SOCKET,
            format!("cannot inspect {}: {err}", path.display()),
        )),
    }
}

async fn accept_loop(
    listener: UnixListener,
    ctx: Arc<Ctx>,
    expected_uid: u32,
    mut stop: watch::Receiver<bool>,
) {
    let mut current: Option<JoinHandle<()>> = None;
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stop.wait_for(|stop| *stop) => break,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(err) => {
                // A transient accept error (for example too many open files) must not end
                // the bridge; the next accept tries again.
                tracing::warn!(error = %err, "accept on the bridge socket failed");
                tokio::task::yield_now().await;
                continue;
            }
        };
        match peer_uid(&stream) {
            Ok(uid) if uid == expected_uid => {}
            Ok(uid) => {
                tracing::error!(
                    code = E_BRIDGE_PEER,
                    peer = uid,
                    own = expected_uid,
                    "a process of another user connected to the bridge socket, refused"
                );
                continue;
            }
            Err(err) => {
                tracing::error!(code = E_BRIDGE_PEER, error = %err, "cannot read the peer uid, refused");
                continue;
            }
        }
        if current
            .as_ref()
            .is_some_and(|session| !session.is_finished())
        {
            tracing::warn!(code = E_BRIDGE_BUSY, "a second core connected, refused");
            tokio::spawn(refuse_busy(stream));
            continue;
        }
        current = Some(tokio::spawn(serve_core(
            stream,
            Arc::clone(&ctx),
            stop.clone(),
        )));
    }
    if let Some(session) = current
        && tokio::time::timeout(SESSION_STOP_WAIT, session)
            .await
            .is_err()
    {
        tracing::warn!("the core session did not close in time");
    }
}

async fn refuse_busy(mut stream: UnixStream) {
    let bye = Frame::Bye {
        code: E_BRIDGE_BUSY.to_owned(),
        message: "another core is connected".to_owned(),
    };
    let _ = write_frame(&mut stream, &bye).await;
}

fn hello(ctx: &Ctx) -> Frame {
    Frame::Hello {
        proto: PROTO_VERSION,
        bridge: BRIDGE_NAME.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: PLATFORM.to_owned(),
        message_limit: MESSAGE_LIMIT,
        hints: DEFAULT_HINTS.to_owned(),
        home: Some(ctx.channel_id.clone()),
        caps: Caps {
            threads: ThreadsCap::Implicit,
            dm: true,
            edit: true,
            files: true,
            status: true,
            reactions: false,
            agent_mode: true,
        },
        spooled: ctx.spool.len() as u64,
    }
}

type Frames = mpsc::UnboundedReceiver<Result<Decoded, ProtoError>>;

/// Reads frames until the stream ends or breaks, so the session loop never waits on a
/// half-read line.
async fn read_frames(
    read: ReadHalf<UnixStream>,
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

async fn write_frames(mut write: WriteHalf<UnixStream>, mut out: mpsc::UnboundedReceiver<Frame>) {
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

/// The `hello_ok` fields a session keeps.
struct Accepted {
    owner_id: String,
    allow: Vec<String>,
    window: usize,
}

async fn serve_core(stream: UnixStream, ctx: Arc<Ctx>, mut stop: watch::Receiver<bool>) {
    let (read, mut write) = tokio::io::split(stream);
    let (frames_tx, mut frames) = mpsc::unbounded_channel();
    let _reader = AbortOnDrop(tokio::spawn(read_frames(read, frames_tx)));
    if let Err(err) = write_frame(&mut write, &hello(&ctx)).await {
        tracing::warn!(error = %err, "could not send hello to the core");
        return;
    }
    let first = tokio::select! {
        first = tokio::time::timeout(SILENCE_LIMIT, frames.recv()) => first,
        _ = stop.wait_for(|stop| *stop) => return,
    };
    let accepted = match first {
        Ok(Some(Ok(Decoded::Frame(Frame::HelloOk {
            proto,
            owner_id,
            allow,
            window,
            ..
        })))) => {
            if proto != PROTO_VERSION {
                let bye = Frame::Bye {
                    code: E_BRIDGE_PROTO.to_owned(),
                    message: format!(
                        "the bridge speaks proto {PROTO_VERSION}, the core sent {proto}"
                    ),
                };
                let _ = write_frame(&mut write, &bye).await;
                tracing::error!(
                    code = E_BRIDGE_PROTO,
                    proto,
                    "the core speaks another proto"
                );
                return;
            }
            Accepted {
                owner_id,
                allow,
                // A window of 0 would stall the spool for good.
                window: window.max(1) as usize,
            }
        }
        Ok(Some(Ok(Decoded::Frame(Frame::Bye { code, .. })))) => {
            tracing::warn!(code = %code, "the core refused the connection");
            return;
        }
        Ok(Some(Ok(_))) | Ok(Some(Err(_))) => {
            tracing::warn!("the core did not answer hello with hello_ok, closing");
            return;
        }
        Ok(None) => {
            tracing::info!("the core closed the link before hello_ok");
            return;
        }
        Err(_) => {
            tracing::warn!("the core sent no hello_ok in time, closing");
            return;
        }
    };
    tracing::info!(window = accepted.window, "core connected");
    ctx.filter.set_access(&accepted.owner_id, &accepted.allow);

    let (out, out_rx) = mpsc::unbounded_channel();
    let mut writer = tokio::spawn(write_frames(write, out_rx));
    let mut session = Session::new(Arc::clone(&ctx), out, accepted);
    session.serve(&mut frames, &mut writer, &mut stop).await;
    session.close();
    // Let the writer flush what is queued (a `bye` included), then close the link.
    drop(session);
    if tokio::time::timeout(SESSION_STOP_WAIT, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
    tracing::info!("core session ended");
}

struct Running {
    task: AbortHandle,
    /// The part file of a download, removed when the request is stopped.
    part: Option<PathBuf>,
}

struct Session {
    ctx: Arc<Ctx>,
    out: mpsc::UnboundedSender<Frame>,
    owner_id: String,
    allow: Vec<String>,
    window: usize,
    /// Spool seqs sent on this connection and not acked yet.
    unacked: BTreeSet<u64>,
    /// The highest spool seq this connection has considered.
    cursor: u64,
    running: HashMap<u64, Running>,
    /// The Slack status per DM thread target, with the number of `start`s not stopped
    /// yet (a lead turn and a lane can work in one thread at once).
    statuses: HashMap<String, (StatusKeeper, usize)>,
    finished_tx: mpsc::UnboundedSender<u64>,
    finished: mpsc::UnboundedReceiver<u64>,
}

impl Session {
    fn new(ctx: Arc<Ctx>, out: mpsc::UnboundedSender<Frame>, accepted: Accepted) -> Self {
        let (finished_tx, finished) = mpsc::unbounded_channel();
        Self {
            ctx,
            out,
            owner_id: accepted.owner_id,
            allow: accepted.allow,
            window: accepted.window,
            unacked: BTreeSet::new(),
            cursor: 0,
            running: HashMap::new(),
            statuses: HashMap::new(),
            finished_tx,
            finished,
        }
    }

    async fn serve(
        &mut self,
        frames: &mut Frames,
        writer: &mut JoinHandle<()>,
        stop: &mut watch::Receiver<bool>,
    ) {
        let mut appended = self.ctx.appended.clone();
        let mut bot_user_id = self.ctx.bot_user_id.clone();
        let mut reconnects = self
            .ctx
            .link
            .reconnects()
            .expect("the slack link counts reconnects");
        // Only Slack reconnects after this point concern this core.
        reconnects.borrow_and_update();
        appended.borrow_and_update();
        bot_user_id.borrow_and_update();
        if let Err(err) = self.ctx.spool.drop_expired(unix_now()) {
            tracing::error!(error = %err, "could not drop expired spool entries");
        }
        self.pump();
        let mut last_heard = Instant::now();
        loop {
            tokio::select! {
                _ = stop.wait_for(|stop| *stop) => {
                    let _ = self.out.send(Frame::Bye {
                        code: E_BRIDGE_STOPPING.to_owned(),
                        message: "the bridge is stopping".to_owned(),
                    });
                    return;
                }
                _ = &mut *writer => {
                    tracing::warn!("the core link stopped taking frames");
                    return;
                }
                _ = tokio::time::sleep_until(last_heard + SILENCE_LIMIT) => {
                    tracing::warn!("the core was silent for {}s, closing the link", SILENCE_LIMIT.as_secs());
                    return;
                }
                Ok(()) = bot_user_id.changed() => self.pump(),
                Ok(()) = appended.changed() => self.pump(),
                Ok(()) = reconnects.changed() => {
                    let _ = self.out.send(Frame::Event { name: "reconnected".to_owned() });
                }
                Some(id) = self.finished.recv() => {
                    self.running.remove(&id);
                }
                next = frames.recv() => {
                    last_heard = Instant::now();
                    match next {
                        None => {
                            tracing::info!("the core closed the link");
                            return;
                        }
                        Some(Ok(decoded)) => {
                            if !self.handle(decoded) {
                                return;
                            }
                        }
                        Some(Err(ProtoError::BadFrame { t, id: Some(id), reason })) => {
                            tracing::warn!(t = %t, id, reason = %reason, "bad frame from the core");
                            self.answer_err(id, E_BRIDGE_BAD_REQUEST, format!("bad `{t}` frame: {reason}"));
                        }
                        Some(Err(err @ (ProtoError::LineTooLong | ProtoError::Io(_)))) => {
                            tracing::warn!(error = %err, "the core link broke");
                            return;
                        }
                        Some(Err(err)) => tracing::warn!(error = %err, "unreadable frame from the core, ignored"),
                    }
                }
            }
        }
    }

    /// Handles one frame from the core; `false` ends the session.
    fn handle(&mut self, decoded: Decoded) -> bool {
        let frame = match decoded {
            Decoded::Frame(frame) => frame,
            Decoded::Unknown { t, id } => {
                tracing::info!(t = %t, ?id, "unknown frame type from the core");
                if let Some(id) = id {
                    self.answer_err(
                        id,
                        E_BRIDGE_UNSUPPORTED,
                        format!("the bridge does not know `{t}` frames"),
                    );
                }
                return true;
            }
        };
        match frame {
            Frame::Ack { seq } => self.ack(seq),
            Frame::Ping { n } => {
                let _ = self.out.send(Frame::Pong { n });
            }
            Frame::Pong { .. } => {}
            Frame::Bye { code, .. } => {
                tracing::info!(code = %code, "the core closed the link");
                return false;
            }
            Frame::Cancel { id } => self.cancel(id),
            Frame::Status { id, target, state } => self.status(id, target, state),
            Frame::React { id, .. } => {
                self.answer_err(
                    id,
                    E_BRIDGE_UNSUPPORTED,
                    "reactions are not supported".into(),
                );
            }
            request @ (Frame::Send { .. }
            | Frame::Edit { .. }
            | Frame::Thread { .. }
            | Frame::Dm { .. }
            | Frame::File { .. }
            | Frame::Fetch { .. }) => self.start(request),
            other => {
                tracing::warn!(t = other.kind(), "unexpected frame from the core");
                if let Some(id) = other.request_id() {
                    self.answer_err(
                        id,
                        E_BRIDGE_UNSUPPORTED,
                        format!("the bridge does not serve `{}` frames", other.kind()),
                    );
                }
            }
        }
        true
    }

    fn answer_err(&self, re: u64, code: &str, message: String) {
        let _ = self.out.send(Frame::Err {
            re,
            code: code.to_owned(),
            message,
        });
    }

    fn ack(&mut self, seq: u64) {
        if !self.unacked.remove(&seq) {
            tracing::debug!(
                seq,
                "ack for a message not open on this connection, ignored"
            );
            return;
        }
        tracing::debug!(seq, "core acked");
        if let Err(err) = self.ctx.spool.remove(seq) {
            tracing::error!(seq, error = %err, "could not remove an acked spool entry");
        }
        self.pump();
    }

    /// Sends spooled messages the core has not seen on this connection, oldest first,
    /// while the window has room. Waits for the bot user id: without it the mapping
    /// cannot tell the bot's own messages apart.
    fn pump(&mut self) {
        let Some(bot_user_id) = self.ctx.bot_user_id.borrow().clone() else {
            return;
        };
        if self.unacked.len() >= self.window {
            return;
        }
        let pending = match self.ctx.spool.pending() {
            Ok(pending) => pending,
            Err(err) => {
                tracing::error!(error = %err, "could not list the spool");
                return;
            }
        };
        let scope = Scope {
            owner_id: self.owner_id.clone(),
            channel_id: self.ctx.channel_id.clone(),
            bot_user_id,
            allowed: self.allow.clone(),
        };
        let cursor = self.cursor;
        for seq in pending.into_iter().filter(|seq| *seq > cursor) {
            if self.unacked.len() >= self.window {
                return;
            }
            self.cursor = seq;
            let (entry, redelivery) = match self.ctx.spool.start_attempt(seq) {
                Ok(Attempt::Send { entry, redelivery }) => (entry, redelivery),
                Ok(Attempt::Exhausted | Attempt::Gone) => continue,
                Err(err) => {
                    tracing::error!(seq, error = %err, "could not read a spool entry");
                    continue;
                }
            };
            let Some(event) = map_event(&entry.payload, &scope) else {
                // Not for this core (another channel, an unlisted sender, a subtype):
                // nothing will ever ack it.
                tracing::debug!(seq, "spooled message not for the core, removed");
                if let Err(err) = self.ctx.spool.remove(seq) {
                    tracing::error!(seq, error = %err, "could not remove a spool entry");
                }
                continue;
            };
            let frame = Frame::Message {
                seq,
                message_id: event.message_id,
                author_id: event.author_id,
                author_is_bot: event.author_is_bot,
                channel_id: event.channel_id,
                thread_id: event.thread_id,
                is_dm: event.is_dm,
                text: Redacted(event.text),
                attachments: event
                    .attachments
                    .into_iter()
                    .map(|a| WireAttachment {
                        name: a.name,
                        size: a.size,
                        mime: a.mime,
                        reference: Redacted(a.reference.0),
                    })
                    .collect(),
                redelivery,
            };
            if frame.encode().len() > MAX_LINE {
                // The core would drop the link on this line, again on every connection.
                tracing::error!(
                    seq,
                    limit = MAX_LINE,
                    "spooled message too large for one line, removed"
                );
                if let Err(err) = self.ctx.spool.remove(seq) {
                    tracing::error!(seq, error = %err, "could not remove a spool entry");
                }
                continue;
            }
            tracing::debug!(seq, redelivery, "message to the core");
            self.unacked.insert(seq);
            let _ = self.out.send(frame);
        }
    }

    /// Runs one core request on its own task; the task answers by `id`.
    fn start(&mut self, request: Frame) {
        let id = request
            .request_id()
            .expect("only requests are started here");
        if self.running.contains_key(&id) {
            self.answer_err(
                id,
                E_BRIDGE_BAD_REQUEST,
                format!("request {id} is already running"),
            );
            return;
        }
        tracing::debug!(t = request.kind(), id, "core request");
        let part = match &request {
            Frame::Fetch { dest, .. } => Some(part_path(dest)),
            _ => None,
        };
        let link = Arc::clone(&self.ctx.link);
        let out = self.out.clone();
        let finished = self.finished_tx.clone();
        let task = tokio::spawn(async move {
            let answer = run_request(&link, request, &out).await;
            let _ = out.send(answer);
            let _ = finished.send(id);
        });
        self.running.insert(
            id,
            Running {
                task: task.abort_handle(),
                part,
            },
        );
    }

    /// Stops request `id` and removes its part file. It gets no answer.
    fn cancel(&mut self, id: u64) {
        match self.running.remove(&id) {
            Some(running) => {
                tracing::info!(id, "core cancelled a request");
                stop_request(running);
            }
            None => tracing::debug!(id, "cancel for a request that is not running"),
        }
    }

    /// Starts or stops the Slack status of `target` and answers `ok` at once: a status
    /// never fails a turn. Targets other than DM threads get no Slack call.
    fn status(&mut self, id: u64, target: String, state: StatusState) {
        match state {
            StatusState::Start => {
                if let Some((_, count)) = self.statuses.get_mut(&target) {
                    *count += 1;
                } else if let Some(keeper) = self.ctx.link.keep_status(&target) {
                    self.statuses.insert(target, (keeper, 1));
                }
            }
            StatusState::Stop => {
                if let Some((_, count)) = self.statuses.get_mut(&target) {
                    *count -= 1;
                    if *count == 0 {
                        // Dropping the keeper clears the status.
                        self.statuses.remove(&target);
                    }
                }
            }
        }
        let _ = self.out.send(Frame::Ok {
            re: id,
            message_id: None,
            thread_id: None,
            target: None,
            bytes: None,
        });
    }

    fn close(&mut self) {
        for (_, running) in self.running.drain() {
            stop_request(running);
        }
        // A core that went away cannot stop its statuses; clear them here.
        self.statuses.clear();
    }
}

fn stop_request(running: Running) {
    running.task.abort();
    if let Some(part) = running.part {
        match fs::remove_file(&part) {
            Ok(()) => tracing::debug!("part file of a stopped download removed"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(error = %err, "could not remove a part file"),
        }
    }
}

/// `<dest>.part`, the file a download writes before its rename.
fn part_path(dest: &Path) -> PathBuf {
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

async fn run_request(
    link: &SlackLink,
    request: Frame,
    out: &mpsc::UnboundedSender<Frame>,
) -> Frame {
    let (id, kind, result) = match request {
        Frame::Send { id, target, text } => (
            id,
            RequestKind::Send,
            link.send(&target, &text.0).await.map(|sent| Answer {
                message_id: Some(sent.message_id),
                ..Answer::default()
            }),
        ),
        Frame::Edit {
            id,
            target,
            message_id,
            text,
        } => (
            id,
            RequestKind::Edit,
            link.edit(&target, &message_id, &text.0)
                .await
                .map(|()| Answer::default()),
        ),
        Frame::Thread {
            id,
            channel_id,
            from_message_id,
            name,
        } => (
            id,
            RequestKind::Thread,
            link.create_thread(&channel_id, &from_message_id, &name.0)
                .await
                .map(|thread_id| Answer {
                    thread_id: Some(thread_id),
                    ..Answer::default()
                }),
        ),
        Frame::Dm { id, user_id } => (
            id,
            RequestKind::Dm,
            link.open_dm(&user_id).await.map(|target| Answer {
                target: Some(target),
                ..Answer::default()
            }),
        ),
        Frame::File {
            id,
            target,
            path,
            name,
        } => (
            id,
            RequestKind::File,
            link.send_file(&target, &path, &name)
                .await
                .map(|sent| Answer {
                    message_id: Some(sent.message_id),
                    ..Answer::default()
                }),
        ),
        Frame::Fetch {
            id,
            attachment,
            dest,
            max_bytes,
        } => (
            id,
            RequestKind::Fetch,
            fetch(link, id, attachment, &dest, max_bytes, out)
                .await
                .map(|bytes| Answer {
                    bytes: Some(bytes),
                    ..Answer::default()
                }),
        ),
        other => unreachable!("`{}` is not started as a request", other.kind()),
    };
    match result {
        Ok(answer) => Frame::Ok {
            re: id,
            message_id: answer.message_id,
            thread_id: answer.thread_id,
            target: answer.target,
            bytes: answer.bytes,
        },
        Err(err) => {
            tracing::info!(id, code = err.code, "core request failed");
            let (code, message) = map_error(kind, err.code, &err.message);
            Frame::Err {
                re: id,
                code: code.to_owned(),
                message,
            }
        }
    }
}

/// The fields of an `ok` answer.
#[derive(Default)]
struct Answer {
    message_id: Option<String>,
    thread_id: Option<String>,
    target: Option<String>,
    bytes: Option<u64>,
}

/// Downloads `attachment` into `dest` under the size limit and the stall guard, sending
/// the running total as `progress` at most every [`PROGRESS_EVERY`] while bytes flow.
async fn fetch(
    link: &SlackLink,
    id: u64,
    attachment: WireAttachment,
    dest: &Path,
    max_bytes: Option<u64>,
    out: &mpsc::UnboundedSender<Frame>,
) -> Result<u64, AdapterError> {
    if !dest.is_absolute() {
        return Err(AdapterError {
            code: E_BRIDGE_BAD_REQUEST,
            message: "`dest` must be an absolute path".to_owned(),
        });
    }
    let progress = Progress::with_limit(max_bytes.unwrap_or(FETCH_CAP));
    let attachment = InboundAttachment {
        name: attachment.name,
        size: attachment.size,
        mime: attachment.mime,
        reference: AttachmentRef(attachment.reference.0),
    };
    let download = files::with_stall_timeout(
        link.fetch_attachment(&attachment, dest, &progress),
        &progress,
    );
    let mut download = std::pin::pin!(download);
    let mut tick = tokio::time::interval(PROGRESS_EVERY);
    let mut reported = 0;
    loop {
        tokio::select! {
            result = &mut download => return result,
            _ = tick.tick() => {
                let bytes = progress.bytes();
                if bytes > reported {
                    reported = bytes;
                    let _ = out.send(Frame::Progress { re: id, bytes });
                }
            }
        }
    }
}
