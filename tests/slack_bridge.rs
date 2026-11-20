//! The Slack bridge on the wire: a core (the real `BridgeAdapter`, or raw frames on the
//! socket) <-> the in-process bridge <-> `FakeSlackTransport`. Offline only. Every wait
//! is on a frame, a recorded Slack call or a ping round trip, bounded by a timeout.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::{
    AttachmentRef, E_ADAPTER_SEND, E_ADAPTER_UPLOAD, E_ADAPTER_UPLOAD_DENIED, InboundAttachment,
    MessengerAdapter,
};
use moraebot::bridge::peer::own_uid;
use moraebot::bridge::proto::{Decoded, Frame, FrameReader, StatusState, ThreadsCap, write_frame};
use moraebot::bridge::{E_BRIDGE_BAD_REQUEST, E_BRIDGE_BUSY, E_BRIDGE_UNSUPPORTED, PROTO_VERSION};
use moraebot::files::{E_FILE_TOO_LARGE, Progress};
use moraebot::slack_bridge::slack::{DEFAULT_HINTS, E_SLACK_UPLOAD, MESSAGE_LIMIT, STATUS_TEXT};
use moraebot::slack_bridge::testing::{
    FakeSlackTransport, SlackFileCall, SlackRecord, StatusCall, TEST_CHANNEL, TestBridge,
    UploadStep,
};
use moraebot::slack_bridge::{BRIDGE_NAME, E_BRIDGE_RUNNING, E_BRIDGE_STOPPING};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);
const OWNER: &str = "U111";

async fn within<T>(future: impl Future<Output = T>) -> T {
    timeout(WAIT, future).await.expect("timed out")
}

async fn bridge(dir: &Path) -> TestBridge {
    TestBridge::start(dir, Arc::new(FakeSlackTransport::new())).await
}

fn message(envelope_id: &str, user: &str, channel: &str, ts: &str) -> Value {
    let channel_type = if channel.starts_with('D') {
        "im"
    } else {
        "channel"
    };
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "payload": {
            "type": "event_callback",
            "event": {
                "type": "message", "user": user, "channel": channel,
                "channel_type": channel_type, "ts": ts, "text": "hello"
            }
        }
    })
}

/// Pushes `frames` into Slack session 1 and waits until the bridge wrote `acks` acks.
async fn from_slack(bridge: &TestBridge, frames: &[Value], acks: usize) {
    let session = within(bridge.fake.session(1)).await;
    for frame in frames {
        session.frame(frame.clone());
    }
    within(
        bridge
            .fake
            .wait_until(|record| record.socket_texts.len() >= acks),
    )
    .await;
}

fn acked_envelopes(fake: &FakeSlackTransport) -> Vec<String> {
    fake.record()
        .socket_texts
        .iter()
        .map(|text| {
            let ack: Value = serde_json::from_str(text).expect("ack is JSON");
            ack["envelope_id"].as_str().expect("envelope id").to_owned()
        })
        .collect()
}

fn spooled(bridge: &TestBridge) -> usize {
    std::fs::read_dir(bridge.spool_dir())
        .expect("spool folder")
        .filter(|entry| {
            entry
                .as_ref()
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".json")
        })
        .count()
}

struct RawCore {
    reader: FrameReader<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    pings: u64,
}

impl RawCore {
    /// Connects and reads the bridge's first frame. A `bye` with `E_BRIDGE_BUSY` means the
    /// previous core's session has not noticed its close yet; that is retried.
    async fn connect(socket: &Path) -> (Self, Frame) {
        within(async {
            loop {
                let (read, writer) = UnixStream::connect(socket)
                    .await
                    .expect("connect")
                    .into_split();
                let mut core = Self {
                    reader: FrameReader::new(BufReader::new(read)),
                    writer,
                    pings: 0,
                };
                match core.next().await {
                    Frame::Bye { code, .. } if code == E_BRIDGE_BUSY => {
                        tokio::task::yield_now().await;
                    }
                    first => return (core, first),
                }
            }
        })
        .await
    }

    async fn accept(socket: &Path, allow: &[&str], window: u32) -> Self {
        let (mut core, hello) = Self::connect(socket).await;
        assert!(matches!(hello, Frame::Hello { .. }), "{hello:?}");
        core.send(&Frame::HelloOk {
            proto: PROTO_VERSION,
            core: "test".to_owned(),
            owner_id: OWNER.to_owned(),
            allow: allow.iter().map(|id| (*id).to_owned()).collect(),
            window,
        })
        .await;
        core
    }

    async fn send(&mut self, frame: &Frame) {
        write_frame(&mut self.writer, frame).await.expect("write");
    }

    async fn send_line(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write");
    }

    /// The next frame, or `None` when the bridge closed the link.
    async fn try_next(&mut self) -> Option<Frame> {
        match within(self.reader.next()).await.expect("readable frame") {
            Some(Decoded::Frame(frame)) => Some(frame),
            Some(Decoded::Unknown { t, .. }) => panic!("unknown frame `{t}` from the bridge"),
            None => None,
        }
    }

    async fn next(&mut self) -> Frame {
        self.try_next().await.expect("the bridge closed the link")
    }

    /// Pings and returns every frame the bridge sent before the pong. The bridge handles
    /// frames in order, so everything the ping follows has been handled by then.
    async fn sync(&mut self) -> Vec<Frame> {
        self.pings += 1;
        let n = self.pings;
        self.send(&Frame::Ping { n }).await;
        let mut before = Vec::new();
        loop {
            match self.next().await {
                Frame::Pong { n: got } if got == n => return before,
                frame => before.push(frame),
            }
        }
    }

    async fn message(&mut self) -> (u64, String, bool) {
        match self.next().await {
            Frame::Message {
                seq,
                message_id,
                redelivery,
                ..
            } => (seq, message_id, redelivery),
            other => panic!("expected a message, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_hello_describes_slack_and_the_adapter_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let (_core, hello) = RawCore::connect(bridge.socket()).await;
    let Frame::Hello {
        proto,
        bridge: name,
        platform,
        message_limit,
        home,
        caps,
        spooled,
        ..
    } = hello
    else {
        panic!("not a hello: {hello:?}");
    };
    assert_eq!(proto, PROTO_VERSION);
    assert_eq!(name, BRIDGE_NAME);
    assert_eq!(platform, "slack");
    assert_eq!(message_limit, MESSAGE_LIMIT);
    assert_eq!(home.as_deref(), Some(TEST_CHANNEL));
    assert_eq!(caps.threads, ThreadsCap::Implicit);
    assert!(caps.dm && caps.edit && caps.files && caps.status && !caps.reactions);
    assert!(caps.agent_mode);
    assert_eq!(spooled, 0);
    drop(_core);

    let adapter = bridge.connect(OWNER, &[]).await;
    assert_eq!(adapter.platform(), "slack");
    assert_eq!(adapter.message_limit(), MESSAGE_LIMIT);
    assert_eq!(adapter.behavior_hints(), DEFAULT_HINTS);
    assert_eq!(adapter.home_target().as_deref(), Some(TEST_CHANNEL));
    adapter.shutdown().await;
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_message_is_spooled_before_its_ack_and_leaves_the_spool_on_the_core_ack() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    from_slack(&bridge, &[message("e1", OWNER, TEST_CHANNEL, "1.1")], 1).await;
    // No core is connected, so the ack proves the message sits in the spool.
    assert_eq!(acked_envelopes(&bridge.fake), ["e1"]);
    assert_eq!(spooled(&bridge), 1);

    let mut core = RawCore::accept(bridge.socket(), &[], 32).await;
    let (seq, message_id, redelivery) = core.message().await;
    assert_eq!(message_id, "C222:1.1");
    assert!(!redelivery);
    core.send(&Frame::Ack { seq }).await;
    assert!(core.sync().await.is_empty());
    assert_eq!(spooled(&bridge), 0);
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_message_that_cannot_be_spooled_is_not_acked() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let spool = bridge.spool_dir();
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o500)).unwrap();
    let other = json!({ "type": "slash_commands", "envelope_id": "e2", "payload": {} });
    from_slack(
        &bridge,
        &[message("e1", OWNER, TEST_CHANNEL, "1.1"), other],
        1,
    )
    .await;
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(acked_envelopes(&bridge.fake), ["e2"]);
    assert_eq!(spooled(&bridge), 0);
    bridge.shutdown().await;
}

#[tokio::test]
async fn an_unacked_message_comes_again_on_the_next_connection_as_a_redelivery() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    from_slack(&bridge, &[message("e1", OWNER, TEST_CHANNEL, "1.1")], 1).await;

    let mut first = RawCore::accept(bridge.socket(), &[], 32).await;
    let (seq, message_id, redelivery) = first.message().await;
    assert!(!redelivery);
    drop(first);

    let mut second = RawCore::accept(bridge.socket(), &[], 32).await;
    let (again, again_id, redelivery) = second.message().await;
    assert_eq!((again, again_id.as_str()), (seq, message_id.as_str()));
    assert!(redelivery);
    bridge.shutdown().await;
}

#[tokio::test]
async fn the_spool_survives_a_bridge_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = bridge(dir.path()).await;
    from_slack(&first, &[message("e1", OWNER, TEST_CHANNEL, "1.1")], 1).await;
    first.shutdown().await;

    let second = bridge(dir.path()).await;
    let adapter = second.connect(OWNER, &[]).await;
    let mut inbound = adapter.start().await.unwrap();
    let event = within(inbound.recv()).await.expect("spooled message");
    assert_eq!(event.message_id, "C222:1.1");
    assert_eq!(event.author_id, OWNER);
    adapter.ack_inbound(&event.message_id).await;
    adapter.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn at_most_window_messages_are_unacked() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let frames: Vec<Value> = ["1.1", "2.2", "3.3"]
        .iter()
        .enumerate()
        .map(|(i, ts)| message(&format!("e{i}"), OWNER, TEST_CHANNEL, ts))
        .collect();
    from_slack(&bridge, &frames, 3).await;

    let mut core = RawCore::accept(bridge.socket(), &[], 2).await;
    let (first, first_id, _) = core.message().await;
    let (_, second_id, _) = core.message().await;
    assert_eq!(
        (first_id.as_str(), second_id.as_str()),
        ("C222:1.1", "C222:2.2")
    );
    assert!(core.sync().await.is_empty(), "the window is full");
    core.send(&Frame::Ack { seq: first }).await;
    let (_, third_id, _) = core.message().await;
    assert_eq!(third_id, "C222:3.3");
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_dm_from_an_unlisted_user_is_removed_at_handover_and_never_sent() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    from_slack(
        &bridge,
        &[
            message("e1", "U999", "D444", "1.1"),
            message("e2", "UPEER", "D555", "2.2"),
            message("e3", OWNER, TEST_CHANNEL, "3.3"),
        ],
        3,
    )
    .await;
    assert_eq!(spooled(&bridge), 3);

    let mut core = RawCore::accept(bridge.socket(), &["UPEER"], 32).await;
    let (peer, peer_id, _) = core.message().await;
    let (owner, owner_id, _) = core.message().await;
    assert_eq!(
        (peer_id.as_str(), owner_id.as_str()),
        ("D555:2.2", "C222:3.3")
    );
    assert!(core.sync().await.is_empty());
    assert_eq!(spooled(&bridge), 2, "the stranger's DM is gone");
    core.send(&Frame::Ack { seq: peer }).await;
    core.send(&Frame::Ack { seq: owner }).await;
    assert!(core.sync().await.is_empty());
    assert_eq!(spooled(&bridge), 0);
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_slack_redelivery_gives_two_frames_with_one_message_id() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let envelope = message("e1", OWNER, TEST_CHANNEL, "1.1");
    from_slack(&bridge, &[envelope.clone(), envelope], 2).await;

    let mut core = RawCore::accept(bridge.socket(), &[], 32).await;
    let (first, first_id, _) = core.message().await;
    let (second, second_id, _) = core.message().await;
    assert_ne!(first, second);
    assert_eq!(first_id, second_id);
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_core_of_another_uid_is_refused_before_hello() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = TestBridge::start_with(
        dir.path(),
        Arc::new(FakeSlackTransport::new()),
        own_uid() + 1,
    )
    .await
    .expect("bridge starts");
    let (read, _write) = UnixStream::connect(bridge.socket())
        .await
        .unwrap()
        .into_split();
    let mut reader = FrameReader::new(BufReader::new(read));
    assert!(within(reader.next()).await.unwrap().is_none());
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_second_core_gets_busy_and_the_first_keeps_its_link() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let mut first = RawCore::accept(bridge.socket(), &[], 32).await;
    assert!(first.sync().await.is_empty());

    let (read, _write) = UnixStream::connect(bridge.socket())
        .await
        .unwrap()
        .into_split();
    let mut second = FrameReader::new(BufReader::new(read));
    match within(second.next()).await.unwrap() {
        Some(Decoded::Frame(Frame::Bye { code, .. })) => assert_eq!(code, E_BRIDGE_BUSY),
        other => panic!("expected bye, got {other:?}"),
    }
    assert!(first.sync().await.is_empty());
    bridge.shutdown().await;
}

#[tokio::test]
async fn the_socket_is_private_a_stale_one_is_replaced_and_a_second_bridge_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let socket = TestBridge::socket_in(dir.path());
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists(), "a stale socket file is left behind");

    let bridge = bridge(dir.path()).await;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);
    let (_core, hello) = RawCore::connect(&socket).await;
    assert!(matches!(hello, Frame::Hello { .. }));

    let second =
        TestBridge::start_with(dir.path(), Arc::new(FakeSlackTransport::new()), own_uid()).await;
    assert_eq!(second.err().expect("refused").code, E_BRIDGE_RUNNING);
    bridge.shutdown().await;
}

#[tokio::test]
async fn requests_reach_slack_and_answers_carry_the_slack_result() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let adapter = bridge.connect(OWNER, &[]).await;

    let sent = adapter.send("C222:1.1", "hi").await.unwrap();
    assert_eq!(sent.message_id, "C222:1.0001");
    adapter
        .edit("C222:1.1", &sent.message_id, "edited")
        .await
        .unwrap();
    assert_eq!(adapter.open_dm(OWNER).await.unwrap(), OWNER);
    assert_eq!(
        adapter
            .create_thread(TEST_CHANNEL, "C222:5.5", "name")
            .await
            .unwrap(),
        "C222:5.5"
    );
    let record = bridge.fake.record();
    assert_eq!(record.posts.len(), 1);
    assert_eq!(record.posts[0]["channel"], TEST_CHANNEL);
    assert_eq!(record.posts[0]["thread_ts"], "1.1");
    assert_eq!(record.updates[0]["ts"], "1.0001");

    let file = dir.path().join("report.txt");
    std::fs::write(&file, b"report").unwrap();
    let uploaded = adapter
        .send_file("C222:1.1", &file, "report.txt")
        .await
        .unwrap();
    assert_eq!(uploaded.message_id, "F111");

    bridge.fake.fail_next_post();
    let err = adapter.send(TEST_CHANNEL, "x").await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_SEND);
    assert!(err.message.contains("channel_not_found"), "{}", err.message);

    bridge
        .fake
        .fail_next_upload_step(UploadStep::UploadUrl, "ratelimited");
    let err = adapter
        .send_file(TEST_CHANNEL, &file, "report.txt")
        .await
        .unwrap_err();
    assert_eq!(err.code, E_ADAPTER_UPLOAD);
    assert!(err.message.starts_with(E_SLACK_UPLOAD), "{}", err.message);

    bridge
        .fake
        .fail_next_upload_step(UploadStep::UploadUrl, "missing_scope");
    let err = adapter
        .send_file(TEST_CHANNEL, &file, "report.txt")
        .await
        .unwrap_err();
    assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);
    adapter.shutdown().await;
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_download_lands_in_dest_and_one_over_the_limit_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let adapter = bridge.connect(OWNER, &[]).await;
    let url = "https://files.slack.com/files-pri/T1-F1/download/a.txt";
    bridge.fake.set_download_bytes(url, b"hello world".to_vec());
    let attachment = InboundAttachment {
        name: "a.txt".to_owned(),
        size: 11,
        mime: Some("text/plain".to_owned()),
        reference: AttachmentRef(url.to_owned()),
    };

    let dest = dir.path().join("a.txt");
    let progress = Progress::new();
    let bytes = adapter
        .fetch_attachment(&attachment, &dest, &progress)
        .await
        .unwrap();
    assert_eq!(bytes, 11);
    assert_eq!(progress.bytes(), 11);
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
    assert_eq!(
        std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let small = dir.path().join("small.txt");
    let err = adapter
        .fetch_attachment(&attachment, &small, &Progress::with_limit(5))
        .await
        .unwrap_err();
    assert_eq!(err.code, E_FILE_TOO_LARGE);
    assert!(!small.exists());
    adapter.shutdown().await;
    bridge.shutdown().await;
}

#[tokio::test]
async fn unknown_bad_and_unsupported_requests_are_answered_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let mut core = RawCore::accept(bridge.socket(), &[], 32).await;
    core.send_line(r#"{"t":"poll","id":4}"#).await;
    core.send_line(r#"{"t":"send","id":5,"text":"no target"}"#)
        .await;
    core.send(&Frame::React {
        id: 6,
        target: "C222:1.1".to_owned(),
        message_id: "C222:1.1".to_owned(),
        emoji: "eyes".to_owned(),
    })
    .await;
    let answers = core.sync().await;
    let codes: Vec<(u64, &str)> = answers
        .iter()
        .map(|frame| match frame {
            Frame::Err { re, code, .. } => (*re, code.as_str()),
            other => panic!("expected err, got {other:?}"),
        })
        .collect();
    assert_eq!(
        codes,
        [
            (4, E_BRIDGE_UNSUPPORTED),
            (5, E_BRIDGE_BAD_REQUEST),
            (6, E_BRIDGE_UNSUPPORTED)
        ]
    );
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_slack_reconnect_is_reported_to_the_core() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let adapter = bridge.connect(OWNER, &[]).await;
    let mut reconnects = adapter
        .reconnects()
        .expect("bridge adapter counts reconnects");
    // A round trip proves the bridge session is serving before Slack says hello.
    adapter.open_dm(OWNER).await.unwrap();
    let before = *reconnects.borrow_and_update();
    let session = within(bridge.fake.session(1)).await;
    session.frame(json!({ "type": "hello", "num_connections": 1 }));
    within(reconnects.changed()).await.unwrap();
    assert_eq!(*reconnects.borrow(), before + 1);
    adapter.shutdown().await;
    bridge.shutdown().await;
}

#[tokio::test]
async fn shutdown_says_bye_and_removes_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let socket = bridge.socket().to_owned();
    let mut core = RawCore::accept(&socket, &[], 32).await;
    assert!(core.sync().await.is_empty());
    let (_, bye) = tokio::join!(bridge.shutdown(), core.next());
    match bye {
        Frame::Bye { code, .. } => assert_eq!(code, E_BRIDGE_STOPPING),
        other => panic!("expected bye, got {other:?}"),
    }
    assert!(core.try_next().await.is_none());
    assert!(!socket.exists());
}

/// A DM in `D333`; `thread_ts` places it inside an existing thread.
fn dm(envelope_id: &str, ts: &str, thread_ts: Option<&str>) -> Value {
    let mut frame = message(envelope_id, OWNER, "D333", ts);
    if let Some(thread_ts) = thread_ts {
        frame["payload"]["event"]["thread_ts"] = json!(thread_ts);
    }
    frame
}

fn status_calls(record: &SlackRecord) -> Vec<(String, String, String)> {
    record
        .statuses
        .iter()
        .map(
            |StatusCall {
                 channel_id,
                 thread_ts,
                 status,
             }| { (channel_id.clone(), thread_ts.clone(), status.clone()) },
        )
        .collect()
}

fn set(thread_ts: &str) -> (String, String, String) {
    (
        "D333".to_owned(),
        thread_ts.to_owned(),
        STATUS_TEXT.to_owned(),
    )
}

fn cleared(thread_ts: &str) -> (String, String, String) {
    ("D333".to_owned(), thread_ts.to_owned(), String::new())
}

#[tokio::test]
async fn a_root_dm_is_its_own_thread_and_the_reply_goes_into_it() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let adapter = bridge.connect(OWNER, &[]).await;
    let mut inbound = adapter.start().await.unwrap();
    from_slack(&bridge, &[dm("e1", "3.3", None)], 1).await;

    let event = within(inbound.recv()).await.expect("the DM arrives");
    assert!(event.is_dm);
    assert_eq!(event.message_id, "D333:3.3");
    assert_eq!(event.thread_id.as_deref(), Some("D333:3.3"));

    adapter
        .send(event.thread_id.as_deref().unwrap(), "answer")
        .await
        .unwrap();
    let post = &bridge.fake.record().posts[0];
    assert_eq!(post["channel"], "D333");
    assert_eq!(
        post["thread_ts"], "3.3",
        "the reply is threaded under the DM"
    );
    adapter.shutdown().await;
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_dm_inside_a_thread_is_answered_in_that_thread() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let adapter = bridge.connect(OWNER, &[]).await;
    let mut inbound = adapter.start().await.unwrap();
    from_slack(&bridge, &[dm("e1", "4.4", Some("3.3"))], 1).await;

    let event = within(inbound.recv()).await.expect("the DM arrives");
    assert_eq!(event.message_id, "D333:4.4");
    assert_eq!(event.thread_id.as_deref(), Some("D333:3.3"));

    adapter
        .send(event.thread_id.as_deref().unwrap(), "answer")
        .await
        .unwrap();
    let post = &bridge.fake.record().posts[0];
    assert_eq!(post["channel"], "D333");
    assert_eq!(post["thread_ts"], "3.3");
    adapter.shutdown().await;
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_dm_turn_sets_the_slack_status_and_its_end_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let adapter = bridge.connect(OWNER, &[]).await;

    let typing = adapter.start_typing("D333:3.3").await.unwrap();
    within(bridge.fake.wait_until(|r| !r.statuses.is_empty())).await;
    assert_eq!(status_calls(&bridge.fake.record()), [set("3.3")]);
    assert!(!STATUS_TEXT.is_empty());

    drop(typing);
    within(bridge.fake.wait_until(|r| r.statuses.len() == 2)).await;
    assert_eq!(
        status_calls(&bridge.fake.record()),
        [set("3.3"), cleared("3.3")]
    );
    adapter.shutdown().await;
    bridge.shutdown().await;
}

#[tokio::test]
async fn a_status_error_still_answers_ok_and_other_targets_call_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let mut core = RawCore::accept(bridge.socket(), &[], 32).await;
    bridge
        .fake
        .fail_next_status("method_not_supported_for_channel_type");

    let start = |id, target: &str| Frame::Status {
        id,
        target: target.to_owned(),
        state: StatusState::Start,
    };
    for (id, target) in [(1, "D333:3.3"), (2, "C222:1.1"), (3, "D333"), (4, OWNER)] {
        core.send(&start(id, target)).await;
        match core.next().await {
            Frame::Ok { re, .. } => assert_eq!(re, id),
            other => panic!("expected ok for {target}, got {other:?}"),
        }
    }
    within(bridge.fake.wait_until(|r| !r.statuses.is_empty())).await;
    assert!(core.sync().await.is_empty());
    // The failed call was the only one: a channel thread, a bare DM channel and a user
    // id get no status.
    assert_eq!(status_calls(&bridge.fake.record()), [set("3.3")]);
    bridge.shutdown().await;
}

#[tokio::test]
async fn overlapping_turns_share_one_status_and_a_lost_core_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let mut core = RawCore::accept(bridge.socket(), &[], 32).await;
    let status = |id, state| Frame::Status {
        id,
        target: "D333:3.3".to_owned(),
        state,
    };

    core.send(&status(1, StatusState::Start)).await;
    core.send(&status(2, StatusState::Start)).await;
    core.send(&status(3, StatusState::Stop)).await;
    let answers = core.sync().await;
    assert_eq!(answers.len(), 3, "{answers:?}");
    within(bridge.fake.wait_until(|r| !r.statuses.is_empty())).await;
    // One start is still open, so the status stays.
    assert_eq!(status_calls(&bridge.fake.record()), [set("3.3")]);

    drop(core);
    within(bridge.fake.wait_until(|r| r.statuses.len() == 2)).await;
    assert_eq!(
        status_calls(&bridge.fake.record()),
        [set("3.3"), cleared("3.3")]
    );
    bridge.shutdown().await;
}

#[tokio::test]
async fn an_upload_into_a_dm_thread_completes_with_the_thread_ts() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = bridge(dir.path()).await;
    let adapter = bridge.connect(OWNER, &[]).await;
    let file = dir.path().join("report.txt");
    std::fs::write(&file, b"report").unwrap();
    adapter
        .send_file("D333:3.3", &file, "report.txt")
        .await
        .unwrap();
    let complete = bridge
        .fake
        .record()
        .file_calls
        .into_iter()
        .find_map(|call| match call {
            SlackFileCall::CompleteUpload {
                channel_id,
                thread_ts,
                ..
            } => Some((channel_id, thread_ts)),
            _ => None,
        })
        .expect("the upload completed");
    assert_eq!(complete, ("D333".to_owned(), Some("3.3".to_owned())));
    adapter.shutdown().await;
    bridge.shutdown().await;
}
