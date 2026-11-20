//! The gateway on `messenger = "bridge"` (issue #40, `docs/bridge.md`): the library-level
//! `Gateway` with a `BridgeAdapter` over `FakeBridge` and `fake-omp`, plus `moraebot run`
//! connecting to a real bridge socket. Nothing reaches a messenger.

use std::fs::OpenOptions;
use std::future::Future;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::MessengerAdapter;
use moraebot::bridge::client::{BridgeAdapter, BridgeOptions, HELLO_CACHE};
use moraebot::bridge::proto::{Decoded, Frame, FrameReader, Redacted, write_frame};
use moraebot::bridge::{E_BRIDGE_PLATFORM, check_peer};
use moraebot::config::{AccessMode, BridgeConfig, Config, MemoryConfig, OmpConfig, Platform};
use moraebot::gateway::{Gateway, GatewayError, STORE_FILE};
use moraebot::onboarding::{self, GreetTarget};
use moraebot::store::{State, Store};
use moraebot::testing::{FakeBridge, FakeBridgeConn, bridge_ok};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const OWNER: &str = "U111";
const DM: &str = "D111";
const CHANNEL: &str = "C222";
const BOUND: Duration = Duration::from_secs(30);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("finished within bound")
}

/// Streams the lines fake-omp appends to `FAKE_OMP_LOG`. The log is a FIFO that the tap
/// holds open for reading and writing, so a test waits for a line instead of polling.
struct LogTap {
    lines: mpsc::UnboundedReceiver<String>,
    prompts: usize,
}

impl LogTap {
    fn create(path: &Path) -> Self {
        let made = Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "mkfifo failed");
        let fifo = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open fifo");
        let (tx, lines) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for line in BufReader::new(fifo).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { lines, prompts: 0 }
    }

    /// Waits until `n` prompts were logged, then checks that no further prompt is
    /// already buffered.
    async fn assert_prompts(&mut self, n: usize) {
        bounded(async {
            while self.prompts < n {
                let line = self.lines.recv().await.expect("log tap open");
                self.note(&line);
            }
        })
        .await;
        while let Ok(line) = self.lines.try_recv() {
            self.note(&line);
        }
        assert_eq!(self.prompts, n, "omp was prompted {} times", self.prompts);
    }

    fn note(&mut self, line: &str) {
        if serde_json::from_str::<Value>(line).is_ok_and(|frame| frame["type"] == "prompt") {
            self.prompts += 1;
        }
    }
}

fn bridge_config(root: &Path, socket: PathBuf, omp_path: PathBuf) -> Config {
    let home = root.join("home");
    let work_dir = root.join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    Config {
        messenger: Platform::Bridge,
        discord: None,
        slack: None,
        bridge: Some(BridgeConfig {
            socket,
            owner_id: OWNER.to_owned(),
            access: Default::default(),
        }),
        work_dir,
        omp_path,
        home,
        access_mode: AccessMode::OwnerOnly,
        omp: OmpConfig::default(),
        memory: MemoryConfig::default(),
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: Config,
    bridge: Arc<FakeBridge>,
    tap: LogTap,
}

struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), GatewayError>>,
}

impl Running {
    async fn stop(self) {
        let _ = self.stop.send(());
        bounded(self.task)
            .await
            .expect("gateway task joined")
            .expect("clean stop");
    }
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_owned();
        let config = bridge_config(
            &root,
            root.join("unused.sock"),
            PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
        );
        let tap = LogTap::create(&root.join("fake-omp.log"));
        Self {
            _dir: dir,
            root,
            config,
            bridge: FakeBridge::new(),
            tap,
        }
    }

    fn options(&self, first_hello_wait: Duration) -> BridgeOptions {
        BridgeOptions {
            first_hello_wait,
            ..BridgeOptions::from_config(&self.config).expect("bridge options")
        }
    }

    /// Opens the adapter against the fake bridge and returns the first connection after
    /// its handshake.
    async fn connected(&self) -> (BridgeAdapter, FakeBridgeConn) {
        let (adapter, conn) = tokio::join!(
            BridgeAdapter::open_with(self.options(Duration::from_secs(5)), self.bridge.clone()),
            async {
                let mut conn = self.bridge.accept().await;
                conn.handshake(&FakeBridge::hello()).await;
                conn
            }
        );
        (adapter.expect("open"), conn)
    }

    /// Runs the gateway on `adapter` with fake-omp answering `echo: <message>`.
    fn start(&self, adapter: BridgeAdapter) -> Running {
        let adapter: Arc<dyn MessengerAdapter> = Arc::new(adapter);
        let vars = [
            ("FAKE_OMP_MODE", "ok".to_owned()),
            (
                "FAKE_OMP_LOG",
                self.root
                    .join("fake-omp.log")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ];
        let gateway = Gateway::open(&self.config, adapter)
            .expect("open gateway")
            .with_omp(|omp| omp.with_extra_env(vars));
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(gateway.run(async {
            let _ = stopped.await;
        }));
        Running { stop, task }
    }

    /// Reads the inbox after the gateway stopped (it holds the store exclusively).
    fn store(&self) -> Store {
        Store::open(&self.config.home.join(STORE_FILE)).expect("reopen store")
    }

    fn row_state(&self, id: i64) -> State {
        self.store()
            .get(id)
            .expect("read row")
            .expect("row exists")
            .state
    }
}

fn message(seq: u64, channel: &str, ts: &str, is_dm: bool, text: &str) -> Frame {
    let id = format!("{channel}:{ts}");
    Frame::Message {
        seq,
        message_id: id.clone(),
        author_id: OWNER.to_owned(),
        author_is_bot: false,
        channel_id: channel.to_owned(),
        // The Slack bridge gives a root DM its own id as thread; a channel root has none.
        thread_id: is_dm.then_some(id),
        is_dm,
        text: Redacted(text.to_owned()),
        attachments: Vec::new(),
        redelivery: false,
    }
}

fn dm(seq: u64, ts: &str, text: &str) -> Frame {
    message(seq, DM, ts, true, text)
}

fn sent(target: &str, text: &str) -> (String, String) {
    (target.to_owned(), text.to_owned())
}

#[tokio::test]
async fn a_message_in_gets_one_send_to_its_thread() {
    let mut rig = Rig::new();
    let (adapter, conn) = rig.connected().await;
    let running = rig.start(adapter);
    let bridge = conn.auto();

    bridge.push(dm(1, "100.0", "hello"));
    bridge.push(message(2, CHANNEL, "200.0", false, "in the channel"));
    bridge
        .wait_until(|r| r.acks == [1, 2] && r.sends().len() == 2)
        .await;

    let record = bridge.record();
    assert_eq!(
        record.sends(),
        vec![
            sent("D111:100.0", "echo: hello"),
            // Implicit threads: the channel message is its own thread, no `thread` frame.
            sent("C222:200.0", "echo: in the channel"),
        ]
    );
    assert!(
        !record
            .requests
            .iter()
            .any(|frame| matches!(frame, Frame::Thread { .. })),
        "no thread request with implicit threads"
    );
    assert!(
        record
            .requests
            .iter()
            .any(|frame| matches!(frame, Frame::Status { .. })),
        "the turn showed a working status"
    );
    running.stop().await;
    assert_eq!(rig.store().count().unwrap(), 2);
    assert_eq!(rig.row_state(1), State::Done);
    assert_eq!(rig.row_state(2), State::Done);
    rig.tap.assert_prompts(2).await;
}

#[tokio::test]
async fn a_reply_cut_off_by_the_bridge_is_sent_once_when_it_returns() {
    let mut rig = Rig::new();
    let (adapter, mut conn) = rig.connected().await;
    let running = rig.start(adapter);

    conn.send(&dm(1, "100.0", "hello")).await;
    // Serve the turn until its reply, then go away without answering it.
    loop {
        match conn.recv().await {
            Frame::Send { target, text, .. } => {
                assert_eq!(sent(&target, &text.0), sent("D111:100.0", "echo: hello"));
                break;
            }
            Frame::Status { id, .. } => conn.send(&bridge_ok(id)).await,
            Frame::Ack { seq } => assert_eq!(seq, 1),
            other => panic!("unexpected {}", other.kind()),
        }
    }
    drop(conn);

    // The reply failed with the link and waits as reply_pending; the next hello_ok
    // bumps reconnects and it goes out once on the new connection.
    let mut conn = rig.bridge.accept().await;
    conn.handshake(&FakeBridge::hello()).await;
    let back = conn.auto();
    back.wait_until(|r| !r.sends().is_empty()).await;
    // A later message is served after the resend, so a second resend would show here.
    back.push(dm(2, "101.0", "marker"));
    back.wait_until(|r| r.sends().len() >= 2).await;
    assert_eq!(
        back.record().sends(),
        vec![
            sent("D111:100.0", "echo: hello"),
            sent("D111:101.0", "echo: marker"),
        ]
    );
    running.stop().await;
    assert_eq!(rig.row_state(1), State::Done);
    assert_eq!(rig.row_state(2), State::Done);
    rig.tap.assert_prompts(2).await;
}

#[tokio::test]
async fn a_message_delivered_twice_and_again_after_a_reconnect_is_handled_once() {
    let mut rig = Rig::new();
    let (adapter, conn) = rig.connected().await;
    let running = rig.start(adapter);

    let first = conn.auto();
    first.push(dm(1, "100.0", "once"));
    first.push(dm(2, "100.0", "once"));
    first
        .wait_until(|r| r.acks == [1, 2] && !r.sends().is_empty())
        .await;
    drop(first);

    let mut conn = rig.bridge.accept().await;
    conn.handshake(&FakeBridge::hello()).await;
    let second = conn.auto();
    let mut again = dm(1, "100.0", "once");
    if let Frame::Message { redelivery, .. } = &mut again {
        *redelivery = true;
    }
    second.push(again);
    // A marker behind the redelivery: any extra turn would be answered before it.
    second.push(dm(2, "101.0", "marker"));
    second
        .wait_until(|r| r.acks == [1, 2] && !r.sends().is_empty())
        .await;
    assert_eq!(
        second.record().sends(),
        vec![sent("D111:101.0", "echo: marker")]
    );
    running.stop().await;
    assert_eq!(
        rig.store().count().unwrap(),
        2,
        "one row for three deliveries plus the marker"
    );
    rig.tap.assert_prompts(2).await;
}

#[tokio::test]
async fn the_gateway_starts_from_the_cached_hello_and_serves_once_the_bridge_appears() {
    let mut rig = Rig::new();
    std::fs::write(
        rig.config.home.join(HELLO_CACHE),
        FakeBridge::hello().encode(),
    )
    .unwrap();
    rig.bridge.set_up(false);
    let adapter =
        BridgeAdapter::open_with(rig.options(Duration::from_millis(200)), rig.bridge.clone())
            .await
            .expect("open from the cached hello");
    let running = rig.start(adapter);

    rig.bridge.set_up(true);
    let mut conn = rig.bridge.accept().await;
    conn.handshake(&FakeBridge::hello()).await;
    let bridge = conn.auto();
    bridge.push(dm(1, "100.0", "hello"));
    bridge
        .wait_until(|r| r.acks == [1] && !r.sends().is_empty())
        .await;
    assert_eq!(
        bridge.record().sends(),
        vec![sent("D111:100.0", "echo: hello")]
    );
    running.stop().await;
    assert_eq!(rig.row_state(1), State::Done);
    rig.tap.assert_prompts(1).await;
}

/// A hello like [`FakeBridge::hello`] without `home`, with the `dm` capability set to
/// `dm_cap`.
fn hello_without_home(dm_cap: bool) -> Frame {
    let Frame::Hello {
        proto,
        bridge,
        version,
        platform,
        message_limit,
        hints,
        mut caps,
        spooled,
        ..
    } = FakeBridge::hello()
    else {
        unreachable!("hello() is a hello");
    };
    caps.dm = dm_cap;
    Frame::Hello {
        proto,
        bridge,
        version,
        platform,
        message_limit,
        hints,
        home: None,
        caps,
        spooled,
    }
}

/// Runs the gateway behind a bridge whose hello has no `home`, with an onboarding request
/// for the owner's DM, until a marker message is answered. Returns every `send` frame.
async fn sends_without_home(dm_cap: bool) -> Vec<(String, String)> {
    let rig = Rig::new();
    onboarding::request(&rig.config.home, GreetTarget::Dm).unwrap();
    let (adapter, conn) = tokio::join!(
        BridgeAdapter::open_with(rig.options(Duration::from_secs(5)), rig.bridge.clone()),
        async {
            let mut conn = rig.bridge.accept().await;
            conn.handshake(&hello_without_home(dm_cap)).await;
            conn
        }
    );
    let running = rig.start(adapter.expect("open"));
    let bridge = conn.auto();
    // The greeting is sent at startup, before the marker is served. The marker's reply
    // carries the onboarding notice, so it is matched by its target.
    bridge.push(dm(1, "100.0", "marker"));
    bridge
        .wait_until(|r| r.sends().iter().any(|(target, _)| target == "D111:100.0"))
        .await;
    let sends = bridge
        .record()
        .requests
        .iter()
        .filter_map(|frame| match frame {
            Frame::Send { target, text, .. } => Some(sent(target, &text.0)),
            _ => None,
        })
        .collect();
    running.stop().await;
    sends
}

#[tokio::test]
async fn without_home_and_dm_the_greeting_sends_no_frame_with_an_empty_target() {
    let sends = sends_without_home(false).await;
    assert!(
        sends.iter().all(|(target, _)| !target.is_empty()),
        "{sends:?}"
    );
    let targets: Vec<&str> = sends.iter().map(|(target, _)| target.as_str()).collect();
    assert_eq!(targets, ["D111:100.0"]);
}

#[tokio::test]
async fn without_home_the_greeting_still_goes_to_the_owner_dm() {
    let sends = sends_without_home(true).await;
    // The fake bridge opens the DM as the owner's id.
    let targets: Vec<&str> = sends.iter().map(|(target, _)| target.as_str()).collect();
    assert_eq!(targets, [OWNER, "D111:100.0"], "{sends:?}");
}

#[tokio::test]
async fn a_bridge_on_another_platform_stops_the_gateway_cleanly() {
    let rig = Rig::new();
    let (adapter, conn) = rig.connected().await;
    let running = rig.start(adapter);
    drop(conn);

    let mut conn = rig.bridge.accept().await;
    let Frame::Hello {
        proto,
        bridge,
        version,
        message_limit,
        hints,
        home,
        caps,
        spooled,
        ..
    } = FakeBridge::hello()
    else {
        unreachable!("hello() is a hello");
    };
    conn.send(&Frame::Hello {
        proto,
        bridge,
        version,
        platform: "discord".to_owned(),
        message_limit,
        hints,
        home,
        caps,
        spooled,
    })
    .await;
    match conn.recv().await {
        Frame::Bye { code, .. } => assert_eq!(code, E_BRIDGE_PLATFORM),
        other => panic!("expected bye, got {}", other.kind()),
    }
    // The adapter closed its inbound channel, so the gateway ends on its own.
    bounded(running.task)
        .await
        .expect("gateway task joined")
        .expect("clean stop");
}

/// Answers the start check of `moraebot run` (`omp_overlay::check`) like omp 18.22.2.
fn omp_stand_in(dir: &Path) -> PathBuf {
    let path = dir.join("omp-stand-in");
    let script = "#!/bin/sh\n\
                  if [ \"$1\" = --version ]; then echo omp/18.22.2; exit 0; fi\n\
                  printf '{\"key\": \"%s\"}\\n' \"$3\"\n";
    std::fs::write(&path, script).expect("write executable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// Kills the child if the test fails before it exited.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn moraebot_run_connects_to_the_bridge_socket_of_its_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let socket = root.join("bridge.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let config = bridge_config(root, socket, omp_stand_in(root));
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, config.to_toml().expect("serialize config")).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_moraebot"))
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .env("HOME", root)
        .env("MORAEBOT_HOME", &config.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn moraebot run");
    let mut child = ChildGuard(child);

    let (stream, _) = bounded(listener.accept())
        .await
        .expect("the core connected");
    check_peer(&stream).expect("the core runs as this user");
    let (read, mut write) = tokio::io::split(stream);
    write_frame(&mut write, &FakeBridge::hello()).await.unwrap();
    let mut frames = FrameReader::new(tokio::io::BufReader::new(read));
    match bounded(frames.next()).await {
        Ok(Some(Decoded::Frame(Frame::HelloOk { owner_id, .. }))) => assert_eq!(owner_id, OWNER),
        other => panic!("expected hello_ok, got {other:?}"),
    }

    assert!(
        config.home.join(HELLO_CACHE).exists(),
        "the hello is cached for the next start"
    );

    // The ack of a dropped stranger message proves the dispatcher runs, so the signal
    // handlers are installed before SIGTERM is sent.
    let mut stranger = dm(1, "100.0", "hi");
    if let Frame::Message { author_id, .. } = &mut stranger {
        *author_id = "U999".to_owned();
    }
    write_frame(&mut write, &stranger).await.unwrap();
    loop {
        match bounded(frames.next()).await {
            Ok(Some(Decoded::Frame(Frame::Ack { seq }))) => {
                assert_eq!(seq, 1);
                break;
            }
            Ok(Some(Decoded::Frame(Frame::Ping { n }))) => {
                write_frame(&mut write, &Frame::Pong { n }).await.unwrap()
            }
            other => panic!("expected ack, got {other:?}"),
        }
    }
    let pid = child.0.id().to_string();
    let killed = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("run kill");
    assert!(killed.success());
    let status = bounded(tokio::task::spawn_blocking(move || {
        let status = child.0.wait();
        (status, child)
    }))
    .await
    .expect("wait task");
    let (status, mut child) = status;
    let status = status.expect("wait for moraebot");
    let mut stderr = String::new();
    if let Some(mut pipe) = child.0.stderr.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert!(status.success(), "moraebot run failed: {stderr}");
}
