//! `BridgeAdapter` against `FakeBridge` (issue #40, `docs/bridge.md`): the handshake,
//! request/answer correlation, capabilities, reconnects, the cached hello, the inbound
//! ack, the link timers, and the dispatcher on top of it.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::adapter::{
    AttachmentRef, E_ADAPTER_DM, E_ADAPTER_EDIT, E_ADAPTER_FILES, E_ADAPTER_SEND, E_ADAPTER_START,
    E_ADAPTER_THREAD, E_ADAPTER_UPLOAD_DENIED, InboundAttachment, InboundEvent, MessengerAdapter,
};
use moraebot::bridge::client::{
    BridgeAdapter, BridgeOptions, HELLO_CACHE, MIN_MESSAGE_LIMIT, REQUEST_DEADLINE, SILENCE_LIMIT,
    UnixConnector, WINDOW,
};
use moraebot::bridge::proto::{Caps, Frame, FrameReader, Redacted, ThreadsCap};
use moraebot::bridge::{
    E_BRIDGE_BAD_REQUEST, E_BRIDGE_DOWN, E_BRIDGE_PLATFORM, E_BRIDGE_PROTO, E_BRIDGE_UNSUPPORTED,
    check_peer, peer,
};
use moraebot::config::{AccessMode, BridgeConfig, Config, MemoryConfig, OmpConfig, Platform};
use moraebot::dispatch::Dispatcher;
use moraebot::files::{E_FILE_FETCH, Progress, with_stall_timeout};
use moraebot::omp::OmpManager;
use moraebot::store::{State, Store};
use moraebot::testing::{
    FakeBridge, FakeBridgeConn, bridge_ok, bridge_ok_bytes, bridge_ok_message, bridge_ok_target,
    bridge_ok_thread,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

const OWNER: &str = "U111";
const FRIEND: &str = "U333";
const DM: &str = "D111";
const BOUND: Duration = Duration::from_secs(30);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("finished within bound")
}

fn options(dir: &Path) -> BridgeOptions {
    BridgeOptions {
        owner_id: OWNER.to_owned(),
        allow: vec![FRIEND.to_owned()],
        hello_cache: dir.join(HELLO_CACHE),
        first_hello_wait: Duration::from_secs(5),
    }
}

/// Opens an adapter against a fresh fake bridge that greets with `hello`.
async fn connected(dir: &Path, hello: Frame) -> (Arc<FakeBridge>, BridgeAdapter, FakeBridgeConn) {
    let bridge = FakeBridge::new();
    let (adapter, conn) = tokio::join!(
        BridgeAdapter::open_with(options(dir), bridge.clone()),
        async {
            let mut conn = bridge.accept().await;
            conn.handshake(&hello).await;
            conn
        }
    );
    (bridge, adapter.expect("open"), conn)
}

/// A root DM from the owner, as the Slack bridge sends it: its thread is itself.
fn dm(seq: u64, ts: &str, text: &str) -> Frame {
    let id = format!("{DM}:{ts}");
    Frame::Message {
        seq,
        message_id: id.clone(),
        author_id: OWNER.to_owned(),
        author_is_bot: false,
        channel_id: DM.to_owned(),
        thread_id: Some(id),
        is_dm: true,
        text: Redacted(text.to_owned()),
        attachments: Vec::new(),
        redelivery: false,
    }
}

fn caps_off() -> Caps {
    Caps::default()
}

/// Waits until the adapter's connect loop has tried again after the link dropped, which
/// means the old session is gone.
async fn wait_for_attempt(bridge: &FakeBridge, after: u64) {
    let mut attempts = bridge.attempts();
    bounded(attempts.wait_for(|n| *n > after))
        .await
        .expect("bridge alive");
}

#[tokio::test]
async fn the_hello_sets_what_the_adapter_reports_and_hello_ok_carries_the_access_list() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = FakeBridge::new();
    let (adapter, hello_ok) = tokio::join!(
        BridgeAdapter::open_with(options(dir.path()), bridge.clone()),
        async {
            let mut conn = bridge.accept().await;
            let answer = conn.handshake(&FakeBridge::hello()).await;
            (conn, answer)
        }
    );
    let adapter = adapter.expect("open");
    let (_conn, hello_ok) = hello_ok;

    assert_eq!(adapter.platform(), "slack");
    assert_eq!(adapter.message_limit(), 3900);
    assert_eq!(adapter.behavior_hints(), "fake bridge hints");
    assert_eq!(adapter.home_target().as_deref(), Some("C222"));
    let Frame::HelloOk {
        proto,
        owner_id,
        allow,
        window,
        ..
    } = hello_ok
    else {
        unreachable!("handshake checked the type");
    };
    assert_eq!(proto, 1);
    assert_eq!(owner_id, OWNER);
    assert_eq!(allow, vec![FRIEND.to_owned()]);
    assert_eq!(window, WINDOW);

    let cache = dir.path().join(HELLO_CACHE);
    let mode = std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let saved = std::fs::read_to_string(&cache).unwrap();
    assert_eq!(saved.as_bytes(), FakeBridge::hello().encode());
}

#[tokio::test]
async fn each_call_writes_its_request_and_maps_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let adapter = Arc::new(adapter);

    let send = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.send("D111:1.0", "done").await }
    });
    let Frame::Send { id, target, text } = conn.recv().await else {
        panic!("send frame");
    };
    assert_eq!((target.as_str(), text.0.as_str()), ("D111:1.0", "done"));
    conn.send(&bridge_ok_message(id, "D111:2.0")).await;
    assert_eq!(bounded(send).await.unwrap().unwrap().message_id, "D111:2.0");

    let edit = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.edit("C222:1.0", "C222:3.0", "halfway").await }
    });
    let Frame::Edit { id, message_id, .. } = conn.recv().await else {
        panic!("edit frame");
    };
    assert_eq!(message_id, "C222:3.0");
    conn.send(&bridge_ok(id)).await;
    bounded(edit).await.unwrap().expect("edited");

    let open_dm = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.open_dm(OWNER).await }
    });
    let Frame::Dm { id, user_id } = conn.recv().await else {
        panic!("dm frame");
    };
    assert_eq!(user_id, OWNER);
    conn.send(&bridge_ok_target(id, DM)).await;
    assert_eq!(bounded(open_dm).await.unwrap().unwrap(), DM);

    let typing = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.start_typing("D111:1.0").await }
    });
    let Frame::Status { id, state, .. } = conn.recv().await else {
        panic!("status frame");
    };
    assert_eq!(state, moraebot::bridge::proto::StatusState::Start);
    conn.send(&bridge_ok(id)).await;
    let handle = bounded(typing).await.unwrap().expect("typing");
    drop(handle);
    let Frame::Status { state, target, .. } = conn.recv().await else {
        panic!("status stop frame");
    };
    assert_eq!(state, moraebot::bridge::proto::StatusState::Stop);
    assert_eq!(target, "D111:1.0");

    // A known code passes through; an unknown one becomes the request's default code.
    let file = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move {
            adapter
                .send_file("C222:1.0", Path::new("/abs/out.pdf"), "out.pdf")
                .await
        }
    });
    let Frame::File { id, path, name, .. } = conn.recv().await else {
        panic!("file frame");
    };
    assert_eq!(
        (path.as_path(), name.as_str()),
        (Path::new("/abs/out.pdf"), "out.pdf")
    );
    conn.send(&Frame::Err {
        re: id,
        code: E_ADAPTER_UPLOAD_DENIED.into(),
        message: "missing files:write".into(),
    })
    .await;
    let err = bounded(file).await.unwrap().unwrap_err();
    assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);

    let send = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.send("C9", "hi").await }
    });
    let Frame::Send { id, .. } = conn.recv().await else {
        panic!("send frame");
    };
    conn.send(&Frame::Err {
        re: id,
        code: "E_SLACK_API".into(),
        message: "channel_not_found".into(),
    })
    .await;
    let err = bounded(send).await.unwrap().unwrap_err();
    assert_eq!(err.code, E_ADAPTER_SEND);
    assert_eq!(err.message, "E_SLACK_API: channel_not_found");
}

#[tokio::test]
async fn unknown_and_bad_frames_from_the_bridge_are_answered_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, _adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;

    conn.send_raw(b"{\"t\":\"poll\",\"id\":4}\n").await;
    let Frame::Err { re, code, .. } = conn.recv().await else {
        panic!("err frame");
    };
    assert_eq!((re, code.as_str()), (4, E_BRIDGE_UNSUPPORTED));

    conn.send_raw(b"{\"t\":\"notice\"}\n").await;
    conn.send_raw(b"{\"t\":\"send\",\"id\":5}\n").await;
    let Frame::Err { re, code, .. } = conn.recv().await else {
        panic!("err frame");
    };
    assert_eq!((re, code.as_str()), (5, E_BRIDGE_BAD_REQUEST));
    conn.assert_quiet().await;
}

#[tokio::test]
async fn capabilities_that_are_off_fail_without_a_frame() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) =
        connected(dir.path(), FakeBridge::hello_with(caps_off())).await;
    let attachment = InboundAttachment {
        name: "a.txt".into(),
        size: 1,
        mime: None,
        reference: AttachmentRef("ref".into()),
    };

    let edit = adapter.edit("C222:1.0", "C222:2.0", "x").await.unwrap_err();
    assert_eq!(edit.code, E_ADAPTER_EDIT);
    let fetch = adapter
        .fetch_attachment(&attachment, &dir.path().join("a.txt"), &Progress::new())
        .await
        .unwrap_err();
    assert_eq!(fetch.code, E_ADAPTER_FILES);
    let file = adapter
        .send_file("C222:1.0", Path::new("/abs/a.txt"), "a.txt")
        .await
        .unwrap_err();
    assert_eq!(file.code, E_ADAPTER_FILES);
    assert_eq!(adapter.open_dm(OWNER).await.unwrap_err().code, E_ADAPTER_DM);
    let thread = adapter
        .create_thread("C222", "C222:1.0", "task")
        .await
        .unwrap_err();
    assert_eq!(thread.code, E_ADAPTER_THREAD);
    drop(
        adapter
            .start_typing("C222:1.0")
            .await
            .expect("a no-op handle"),
    );
    conn.assert_quiet().await;
}

#[tokio::test]
async fn implicit_threads_need_no_frame_and_create_threads_ask_the_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let thread = adapter
        .create_thread("C222", "C222:1.0", "task")
        .await
        .expect("implicit thread");
    assert_eq!(thread, "C222:1.0");
    conn.assert_quiet().await;

    let dir = tempfile::tempdir().unwrap();
    let caps = Caps {
        threads: ThreadsCap::Create,
        ..caps_off()
    };
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello_with(caps)).await;
    let adapter = Arc::new(adapter);
    let create = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move {
            adapter
                .create_thread("C222", "C222:1.0", "fix the build")
                .await
        }
    });
    let Frame::Thread {
        id,
        channel_id,
        from_message_id,
        name,
    } = conn.recv().await
    else {
        panic!("thread frame");
    };
    assert_eq!(
        (
            channel_id.as_str(),
            from_message_id.as_str(),
            name.0.as_str()
        ),
        ("C222", "C222:1.0", "fix the build")
    );
    conn.send(&bridge_ok_thread(id, "T1")).await;
    assert_eq!(bounded(create).await.unwrap().unwrap(), "T1");
}

#[tokio::test]
async fn reconnects_go_up_on_every_hello_ok_and_on_a_messenger_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let mut reconnects = adapter.reconnects().expect("the bridge adapter reconnects");
    assert_eq!(*reconnects.borrow_and_update(), 1, "the first hello_ok");

    conn.send(&Frame::Event {
        name: "reconnected".into(),
    })
    .await;
    bounded(reconnects.wait_for(|n| *n == 2)).await.unwrap();

    // The link drops: sends fail at once, and the next hello_ok bumps the counter, so
    // the dispatcher re-sends its pending replies.
    bridge.set_up(false);
    let before = *bridge.attempts().borrow();
    drop(conn);
    wait_for_attempt(&bridge, before).await;
    let err = adapter.send("D111:1.0", "reply").await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_SEND);
    assert!(err.message.starts_with(E_BRIDGE_DOWN), "{}", err.message);
    assert_eq!(*reconnects.borrow(), 2);

    bridge.set_up(true);
    let adapter = Arc::new(adapter);
    let (_, mut conn) = tokio::join!(bounded(reconnects.wait_for(|n| *n == 3)), async {
        let mut conn = bridge.accept().await;
        conn.handshake(&FakeBridge::hello()).await;
        conn
    });
    let resend = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.send("D111:1.0", "reply").await }
    });
    let Frame::Send { id, text, .. } = conn.recv().await else {
        panic!("send frame");
    };
    assert_eq!(text.0, "reply");
    conn.send(&bridge_ok_message(id, "D111:2.0")).await;
    bounded(resend)
        .await
        .unwrap()
        .expect("sent after the reconnect");
}

#[tokio::test]
async fn a_request_in_flight_fails_when_the_link_drops() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    bridge.set_up(false);
    let adapter = Arc::new(adapter);
    let send = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.send("D111:1.0", "reply").await }
    });
    assert!(matches!(conn.recv().await, Frame::Send { .. }));
    drop(conn);
    let err = bounded(send).await.unwrap().unwrap_err();
    assert_eq!(err.code, E_ADAPTER_SEND);
    assert!(err.message.contains(E_BRIDGE_DOWN), "{}", err.message);
}

#[tokio::test]
async fn with_the_bridge_down_the_core_starts_from_the_cached_hello() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(HELLO_CACHE), FakeBridge::hello().encode()).unwrap();
    let bridge = FakeBridge::new();
    bridge.set_up(false);
    let options = BridgeOptions {
        first_hello_wait: Duration::from_millis(200),
        ..options(dir.path())
    };
    let adapter = BridgeAdapter::open_with(options, bridge.clone())
        .await
        .expect("open from the cache");
    assert_eq!(adapter.platform(), "slack");
    assert_eq!(adapter.home_target().as_deref(), Some("C222"));
    assert_eq!(adapter.message_limit(), 3900);
    let mut inbound = adapter.start().await.expect("a receiver");
    let err = adapter.send("D111:1.0", "hi").await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_SEND);
    assert!(err.message.starts_with(E_BRIDGE_DOWN), "{}", err.message);

    // The bridge appears later and its messages flow.
    bridge.set_up(true);
    let mut conn = bridge.accept().await;
    conn.handshake(&FakeBridge::hello()).await;
    conn.send(&dm(1, "100.0", "hello")).await;
    let event = bounded(inbound.recv()).await.expect("event");
    assert_eq!(event.text, "hello");
    assert_eq!(event.thread_id.as_deref(), Some("D111:100.0"));
}

#[tokio::test]
async fn with_no_bridge_and_no_cached_hello_open_fails() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = FakeBridge::new();
    bridge.set_up(false);
    let options = BridgeOptions {
        first_hello_wait: Duration::from_millis(100),
        ..options(dir.path())
    };
    let Err(err) = BridgeAdapter::open_with(options, bridge).await else {
        panic!("open must fail");
    };
    assert_eq!(err.code, E_ADAPTER_START);
    assert!(err.message.starts_with(E_BRIDGE_DOWN), "{}", err.message);
}

#[tokio::test]
async fn requests_and_progress_match_by_id_while_inbound_keeps_flowing() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let adapter = Arc::new(adapter);
    let mut inbound = adapter.start().await.unwrap();
    let progress = Arc::new(Progress::with_limit(1000));
    let dest = dir.path().join("report.pdf");

    let fetch = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        let progress = Arc::clone(&progress);
        let dest = dest.clone();
        async move {
            let attachment = InboundAttachment {
                name: "report.pdf".into(),
                size: 300,
                mime: Some("application/pdf".into()),
                reference: AttachmentRef("https://files.example/F1".into()),
            };
            adapter
                .fetch_attachment(&attachment, &dest, &progress)
                .await
        }
    });
    let Frame::Fetch {
        id: fetch_id,
        attachment,
        dest: asked,
        max_bytes,
    } = conn.recv().await
    else {
        panic!("fetch frame");
    };
    assert_eq!(attachment.reference.0, "https://files.example/F1");
    assert_eq!((asked, max_bytes), (dest, Some(1000)));

    let send = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.send("D111:1.0", "meanwhile").await }
    });
    let Frame::Send { id: send_id, .. } = conn.recv().await else {
        panic!("send frame");
    };
    assert_ne!(fetch_id, send_id);

    // The later request is answered first; then progress and a new message arrive while
    // the download is still running.
    conn.send(&bridge_ok_message(send_id, "D111:2.0")).await;
    assert_eq!(bounded(send).await.unwrap().unwrap().message_id, "D111:2.0");
    conn.send(&Frame::Progress {
        re: fetch_id,
        bytes: 100,
    })
    .await;
    conn.send(&dm(1, "200.0", "while downloading")).await;
    let event = bounded(inbound.recv())
        .await
        .expect("event during the download");
    assert_eq!(event.text, "while downloading");
    assert!(!fetch.is_finished());
    conn.send(&Frame::Progress {
        re: fetch_id,
        bytes: 300,
    })
    .await;
    conn.send(&bridge_ok_bytes(fetch_id, 300)).await;
    assert_eq!(bounded(fetch).await.unwrap().unwrap(), 300);
    assert_eq!(progress.bytes(), 300, "running totals became deltas");

    // The message is acked only once the core handled it.
    conn.assert_quiet().await;
    adapter.ack_inbound(&event.message_id).await;
    assert_eq!(conn.recv().await, Frame::Ack { seq: 1 });
}

#[tokio::test]
async fn progress_past_the_limit_cancels_the_download() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let adapter = Arc::new(adapter);
    let fetch = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        let dest = dir.path().join("big.bin");
        async move {
            let attachment = InboundAttachment {
                name: "big.bin".into(),
                size: 10,
                mime: None,
                reference: AttachmentRef("ref".into()),
            };
            adapter
                .fetch_attachment(&attachment, &dest, &Progress::with_limit(10))
                .await
        }
    });
    let Frame::Fetch { id, .. } = conn.recv().await else {
        panic!("fetch frame");
    };
    conn.send(&Frame::Progress { re: id, bytes: 11 }).await;
    let err = bounded(fetch).await.unwrap().unwrap_err();
    assert_eq!(err.code, moraebot::files::E_FILE_TOO_LARGE);
    assert_eq!(conn.recv().await, Frame::Cancel { id });
}

#[tokio::test]
async fn a_final_ok_past_the_limit_is_refused_without_progress() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let adapter = Arc::new(adapter);
    let progress = Arc::new(Progress::with_limit(10));
    let fetch = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        let progress = Arc::clone(&progress);
        let dest = dir.path().join("big.bin");
        async move {
            let attachment = InboundAttachment {
                name: "big.bin".into(),
                size: 10,
                mime: None,
                reference: AttachmentRef("ref".into()),
            };
            adapter
                .fetch_attachment(&attachment, &dest, &progress)
                .await
        }
    });
    let Frame::Fetch { id, .. } = conn.recv().await else {
        panic!("fetch frame");
    };
    conn.send(&bridge_ok_bytes(id, 11)).await;
    let err = bounded(fetch).await.unwrap().unwrap_err();
    assert_eq!(err.code, moraebot::files::E_FILE_TOO_LARGE);
    // The bridge already answered, so there is nothing to cancel.
    conn.assert_quiet().await;

    // Within the limit, the final total is recorded like a progress frame.
    let progress = Arc::new(Progress::with_limit(100));
    let fetch = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        let progress = Arc::clone(&progress);
        let dest = dir.path().join("small.bin");
        async move {
            let attachment = InboundAttachment {
                name: "small.bin".into(),
                size: 40,
                mime: None,
                reference: AttachmentRef("ref".into()),
            };
            adapter
                .fetch_attachment(&attachment, &dest, &progress)
                .await
        }
    });
    let Frame::Fetch { id, .. } = conn.recv().await else {
        panic!("fetch frame");
    };
    conn.send(&Frame::Progress { re: id, bytes: 15 }).await;
    conn.send(&bridge_ok_bytes(id, 40)).await;
    assert_eq!(bounded(fetch).await.unwrap().unwrap(), 40);
    assert_eq!(progress.bytes(), 40);
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_request_fails_after_its_deadline_and_typing_never_waits() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let adapter = Arc::new(adapter);

    // `status start` is fire and forget: the handle comes back before any answer.
    let started = Instant::now();
    let typing = bounded(adapter.start_typing("D111:1.0"))
        .await
        .expect("typing");
    assert_eq!(started.elapsed(), Duration::ZERO);
    let Frame::Status { state, .. } = conn.recv().await else {
        panic!("status frame");
    };
    assert_eq!(state, moraebot::bridge::proto::StatusState::Start);
    drop(typing);
    let Frame::Status { state, .. } = conn.recv().await else {
        panic!("status stop frame");
    };
    assert_eq!(state, moraebot::bridge::proto::StatusState::Stop);

    // The fake answers pings (inside `recv`) but never the send.
    let send = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move {
            let started = Instant::now();
            (adapter.send("D111:1.0", "reply").await, started.elapsed())
        }
    });
    let Frame::Send { id, .. } = conn.recv().await else {
        panic!("send frame");
    };
    assert_eq!(conn.recv().await, Frame::Cancel { id });
    let (result, elapsed) = tokio::time::timeout(REQUEST_DEADLINE * 2, send)
        .await
        .expect("the send ended")
        .unwrap();
    let err = result.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_SEND);
    assert!(err.message.contains("no answer"), "{}", err.message);
    assert_eq!(elapsed, REQUEST_DEADLINE);
}

#[tokio::test]
async fn a_bridge_that_overruns_the_window_is_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let mut inbound = adapter.start().await.unwrap();
    let before = *bridge.attempts().borrow();
    for seq in 1..=u64::from(WINDOW) + 1 {
        conn.send(&dm(seq, &format!("{seq}.0"), "burst")).await;
    }
    let written = conn.until_closed().await;
    assert!(written.is_empty(), "{written:?}");
    wait_for_attempt(&bridge, before).await;
    // The window's worth was delivered; the extra one stays in the bridge's spool.
    for seq in 1..=u64::from(WINDOW) {
        let event = bounded(inbound.recv()).await.expect("event");
        assert_eq!(event.message_id, format!("{DM}:{seq}.0"));
    }
    assert!(
        inbound.try_recv().is_err(),
        "the overrun message was not delivered"
    );
}

#[tokio::test]
async fn an_unusable_hello_gets_bye_and_an_unusable_cache_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, adapter, conn) = connected(dir.path(), FakeBridge::hello()).await;
    let mut reconnects = adapter.reconnects().unwrap();
    drop(conn);

    let breaks: [fn(&mut Frame); 3] = [
        |hello: &mut Frame| {
            if let Frame::Hello { message_limit, .. } = hello {
                *message_limit = 0;
            }
        },
        |hello: &mut Frame| {
            if let Frame::Hello { message_limit, .. } = hello {
                *message_limit = MIN_MESSAGE_LIMIT - 1;
            }
        },
        |hello: &mut Frame| {
            if let Frame::Hello { platform, .. } = hello {
                platform.clear();
            }
        },
    ];
    for broken in breaks {
        let mut conn = bridge.accept().await;
        let mut hello = FakeBridge::hello();
        broken(&mut hello);
        conn.send(&hello).await;
        let Some(Frame::Bye { code, .. }) = conn.next_frame().await else {
            panic!("bye");
        };
        assert_eq!(code, E_BRIDGE_BAD_REQUEST);
    }
    let mut conn = bridge.accept().await;
    conn.handshake(&FakeBridge::hello()).await;
    bounded(reconnects.wait_for(|n| *n == 2)).await.unwrap();
    assert_eq!(adapter.message_limit(), 3900);
    drop(conn);
    drop(adapter);

    // A cached hello of another proto is not a start point.
    let cached = tempfile::tempdir().unwrap();
    let mut hello = FakeBridge::hello();
    if let Frame::Hello { proto, .. } = &mut hello {
        *proto = 2;
    }
    std::fs::write(cached.path().join(HELLO_CACHE), hello.encode()).unwrap();
    let bridge = FakeBridge::new();
    bridge.set_up(false);
    let options = BridgeOptions {
        first_hello_wait: Duration::from_millis(100),
        ..options(cached.path())
    };
    let Err(err) = BridgeAdapter::open_with(options, bridge).await else {
        panic!("open must fail");
    };
    assert_eq!(err.code, E_ADAPTER_START);
}

#[tokio::test(start_paused = true)]
async fn the_stall_watchdog_fires_when_progress_stops_and_the_fetch_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let adapter = Arc::new(adapter);
    let fetch = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        let dest = dir.path().join("slow.bin");
        async move {
            let attachment = InboundAttachment {
                name: "slow.bin".into(),
                size: 1000,
                mime: None,
                reference: AttachmentRef("ref".into()),
            };
            let progress = Progress::new();
            let started = Instant::now();
            let result = with_stall_timeout(
                adapter.fetch_attachment(&attachment, &dest, &progress),
                &progress,
            )
            .await;
            (result, started.elapsed())
        }
    });
    let Frame::Fetch { id, .. } = conn.recv().await else {
        panic!("fetch frame");
    };
    conn.send(&Frame::Progress { re: id, bytes: 10 }).await;
    // The fake keeps answering pings while it waits, so only the stall watchdog fires.
    assert_eq!(conn.recv().await, Frame::Cancel { id });
    let (result, elapsed) = fetch.await.unwrap();
    assert_eq!(result.unwrap_err().code, E_FILE_FETCH);
    assert!(elapsed >= moraebot::files::STALL_TIMEOUT, "{elapsed:?}");
}

#[tokio::test]
async fn another_proto_gets_bye_and_no_bump() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, adapter, conn) = connected(dir.path(), FakeBridge::hello()).await;
    let mut reconnects = adapter.reconnects().unwrap();
    drop(conn);

    let mut conn = bridge.accept().await;
    let Frame::Hello {
        bridge: name,
        version,
        platform,
        message_limit,
        hints,
        home,
        caps,
        spooled,
        ..
    } = FakeBridge::hello()
    else {
        unreachable!();
    };
    conn.send(&Frame::Hello {
        proto: 2,
        bridge: name,
        version,
        platform,
        message_limit,
        hints,
        home,
        caps,
        spooled,
    })
    .await;
    let Some(Frame::Bye { code, .. }) = conn.next_frame().await else {
        panic!("bye");
    };
    assert_eq!(code, E_BRIDGE_PROTO);

    let mut conn = bridge.accept().await;
    conn.handshake(&FakeBridge::hello()).await;
    bounded(reconnects.wait_for(|n| *n == 2)).await.unwrap();
    assert_eq!(*reconnects.borrow(), 2, "the refused hello did not count");
}

#[tokio::test]
async fn another_platform_stops_the_adapter_and_closes_inbound() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, adapter, conn) = connected(dir.path(), FakeBridge::hello()).await;
    let mut inbound = adapter.start().await.unwrap();
    drop(conn);

    let mut conn = bridge.accept().await;
    let mut hello = FakeBridge::hello();
    if let Frame::Hello { platform, .. } = &mut hello {
        *platform = "discord".into();
    }
    conn.send(&hello).await;
    let Some(Frame::Bye { code, .. }) = conn.next_frame().await else {
        panic!("bye");
    };
    assert_eq!(code, E_BRIDGE_PLATFORM);
    assert_eq!(bounded(inbound.recv()).await, None, "the gateway stops");
    assert_eq!(adapter.platform(), "slack");
}

#[tokio::test]
async fn a_window_of_messages_while_a_send_waits_does_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let adapter = Arc::new(adapter);
    // Nobody reads the receiver until the send is done.
    let mut inbound = adapter.start().await.unwrap();
    let send = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.send("D111:1.0", "reply").await }
    });
    let Frame::Send { id, .. } = conn.recv().await else {
        panic!("send frame");
    };
    for seq in 1..=u64::from(WINDOW) {
        conn.send(&dm(seq, &format!("{seq}.0"), "burst")).await;
    }
    conn.send(&bridge_ok_message(id, "D111:9.0")).await;
    bounded(send).await.unwrap().expect("the send got its ok");
    for seq in 1..=u64::from(WINDOW) {
        let event = bounded(inbound.recv()).await.expect("event");
        assert_eq!(event.message_id, format!("{DM}:{seq}.0"));
    }
}

#[tokio::test(start_paused = true)]
async fn reconnects_back_off_from_half_a_second_to_ten() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(HELLO_CACHE), FakeBridge::hello().encode()).unwrap();
    let bridge = FakeBridge::new();
    bridge.set_up(false);
    let mut attempts = bridge.attempts();
    let options = BridgeOptions {
        first_hello_wait: Duration::from_secs(1),
        ..options(dir.path())
    };
    let (adapter, times) = tokio::join!(BridgeAdapter::open_with(options, bridge.clone()), async {
        let mut times = Vec::new();
        while times.len() < 8 {
            attempts.changed().await.unwrap();
            times.push(Instant::now());
        }
        times
    });
    let _adapter = adapter.expect("open from the cache");
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    let ms = Duration::from_millis;
    assert_eq!(
        gaps,
        vec![
            ms(500),
            ms(1000),
            ms(2000),
            ms(4000),
            ms(8000),
            ms(10_000),
            ms(10_000)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_silent_bridge_is_dropped_after_45_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, _adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let before = *bridge.attempts().borrow();
    let start = Instant::now();
    // The fake reads but never answers the pings.
    let written = conn.until_closed().await;
    let elapsed = start.elapsed();
    assert!(written.is_empty(), "{written:?}");
    assert!(
        elapsed >= SILENCE_LIMIT && elapsed < SILENCE_LIMIT + Duration::from_secs(1),
        "{elapsed:?}"
    );
    wait_for_attempt(&bridge, before).await;
}

#[tokio::test]
async fn the_peer_uid_check_passes_over_a_real_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("slack-bridge.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (core_uid_tx, core_uid) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        check_peer(&stream).expect("the bridge accepts the core");
        let _ = core_uid_tx.send(peer::peer_uid(&stream).unwrap());
        let (read, mut write) = tokio::io::split(stream);
        moraebot::bridge::proto::write_frame(&mut write, &FakeBridge::hello())
            .await
            .unwrap();
        let mut frames = FrameReader::new(tokio::io::BufReader::new(read));
        let answer = frames.next().await.unwrap().expect("hello_ok");
        (answer, frames, write)
    });
    let config = bridge_config(dir.path(), socket);
    let adapter = BridgeAdapter::open(&config)
        .await
        .expect("open over the socket");
    assert_eq!(adapter.platform(), "slack");
    assert_eq!(bounded(core_uid).await.unwrap(), peer::own_uid());
    let (answer, _frames, _write) = bounded(server).await.unwrap();
    assert!(matches!(
        answer,
        moraebot::bridge::proto::Decoded::Frame(Frame::HelloOk { .. })
    ));

    let missing = UnixConnector::new(dir.path().join("none.sock"));
    let err = moraebot::bridge::client::Connector::connect(&missing)
        .await
        .err()
        .expect("no socket");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

// The dispatcher on top of the bridge adapter.

fn bridge_config(root: &Path, socket: PathBuf) -> Config {
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
        omp_path: PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
        home,
        access_mode: AccessMode::OwnerOnly,
        omp: OmpConfig::default(),
        memory: MemoryConfig::default(),
    }
}

struct Core {
    store: Arc<Mutex<Store>>,
    dispatcher: Arc<Dispatcher>,
    runner: tokio::task::JoinHandle<()>,
}

impl Core {
    /// A dispatcher on a bridge adapter, with fake-omp answering `echo: <message>`.
    async fn open(root: &Path, bridge: &Arc<FakeBridge>) -> (Self, FakeBridgeConn) {
        let config = bridge_config(root, root.join("unused.sock"));
        let options = BridgeOptions {
            first_hello_wait: Duration::from_secs(5),
            ..BridgeOptions::from_config(&config).unwrap()
        };
        let (adapter, conn) =
            tokio::join!(BridgeAdapter::open_with(options, bridge.clone()), async {
                let mut conn = bridge.accept().await;
                conn.handshake(&FakeBridge::hello()).await;
                conn
            });
        let adapter: Arc<dyn MessengerAdapter> = Arc::new(adapter.expect("open"));
        let store = Arc::new(Mutex::new(
            Store::open(&config.home.join("inbox.sqlite")).unwrap(),
        ));
        let omp = OmpManager::new(config.clone(), Arc::clone(&store)).with_extra_env([
            ("FAKE_OMP_MODE", "ok".to_owned()),
            (
                "FAKE_OMP_LOG",
                root.join("fake-omp.log").to_string_lossy().into_owned(),
            ),
        ]);
        let dispatcher = Arc::new(Dispatcher::new(
            &config,
            Arc::clone(&adapter),
            Arc::clone(&store),
            omp,
        ));
        let inbound = adapter.start().await.unwrap();
        let runner = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            async move {
                dispatcher.run(inbound).await.expect("dispatcher ran");
            }
        });
        (
            Self {
                store,
                dispatcher,
                runner,
            },
            conn,
        )
    }

    fn rows(&self) -> i64 {
        self.store.lock().unwrap().count().unwrap()
    }

    fn state(&self, id: i64) -> State {
        self.store
            .lock()
            .unwrap()
            .get(id)
            .unwrap()
            .expect("row")
            .state
    }

    async fn close(self) {
        self.runner.abort();
        self.dispatcher.shutdown().await;
    }
}

#[tokio::test]
async fn a_spooled_message_delivered_twice_makes_one_inbox_row() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = FakeBridge::new();
    let (core, conn) = Core::open(dir.path(), &bridge).await;
    let first = conn.auto();
    first.push(dm(1, "100.0", "hello"));
    first.push(dm(2, "100.0", "hello"));
    first
        .wait_until(|r| r.acks == [1, 2] && r.sends().len() == 1)
        .await;
    assert_eq!(
        first.record().sends(),
        vec![("D111:100.0".to_owned(), "echo: hello".to_owned())]
    );
    drop(first);

    // Redelivered after a reconnect: handled as a duplicate and acked again.
    let mut conn = bridge.accept().await;
    conn.handshake(&FakeBridge::hello()).await;
    let second = conn.auto();
    let mut again = dm(1, "100.0", "hello");
    if let Frame::Message { redelivery, .. } = &mut again {
        *redelivery = true;
    }
    second.push(again);
    second.wait_until(|r| r.acks == [1]).await;
    assert_eq!(core.rows(), 1, "one inbox row for three deliveries");
    assert!(second.record().sends().is_empty());
    core.close().await;
}

#[tokio::test]
async fn a_reply_that_failed_while_the_bridge_was_away_is_sent_after_it_returns() {
    let dir = tempfile::tempdir().unwrap();
    let bridge = FakeBridge::new();
    let (core, mut conn) = Core::open(dir.path(), &bridge).await;
    conn.send(&dm(1, "100.0", "hello")).await;
    // Answer everything until the reply, which the messenger refuses.
    let reply = loop {
        match conn.recv().await {
            Frame::Send { id, target, text } => break (id, target, text.0),
            Frame::Status { id, .. } | Frame::Edit { id, .. } => conn.send(&bridge_ok(id)).await,
            Frame::Ack { seq } => assert_eq!(seq, 1),
            other => panic!("unexpected {}", other.kind()),
        }
    };
    assert_eq!(
        (reply.1.as_str(), reply.2.as_str()),
        ("D111:100.0", "echo: hello")
    );
    conn.send(&Frame::Err {
        re: reply.0,
        code: E_ADAPTER_SEND.into(),
        message: "E_SLACK_API: service unavailable".into(),
    })
    .await;
    drop(conn);

    // The bridge comes back: the hello_ok bumps reconnects and the reply goes out once.
    let mut conn = bridge.accept().await;
    conn.handshake(&FakeBridge::hello()).await;
    let back = conn.auto();
    back.wait_until(|r| r.sends().len() == 1).await;
    // A later message in the same thread is served after the resend settled.
    back.push(dm(2, "101.0", "second"));
    back.wait_until(|r| r.sends().len() == 2).await;
    assert_eq!(
        back.record().sends(),
        vec![
            ("D111:100.0".to_owned(), "echo: hello".to_owned()),
            ("D111:101.0".to_owned(), "echo: second".to_owned()),
        ]
    );
    assert_eq!(core.state(1), State::Done);
    assert_eq!(core.rows(), 2);
    core.close().await;
}

#[tokio::test]
async fn an_inbound_event_maps_every_field() {
    let dir = tempfile::tempdir().unwrap();
    let (_bridge, adapter, mut conn) = connected(dir.path(), FakeBridge::hello()).await;
    let mut inbound: mpsc::Receiver<InboundEvent> = adapter.start().await.unwrap();
    conn.send_raw(
        br#"{"t":"message","seq":41,"message_id":"C222:1.0","author_id":"B444","author_is_bot":true,"channel_id":"C222","thread_id":null,"is_dm":false,"text":"hi","attachments":[{"name":"r.pdf","size":5,"mime":"application/pdf","ref":"https://files.example/F1"}],"redelivery":true}
"#,
    )
    .await;
    let event = bounded(inbound.recv()).await.unwrap();
    assert_eq!(
        event,
        InboundEvent {
            message_id: "C222:1.0".into(),
            author_id: "B444".into(),
            author_is_bot: true,
            channel_id: "C222".into(),
            thread_id: None,
            is_dm: false,
            text: "hi".into(),
            attachments: vec![InboundAttachment {
                name: "r.pdf".into(),
                size: 5,
                mime: Some("application/pdf".into()),
                reference: AttachmentRef("https://files.example/F1".into()),
            }],
        }
    );
}
