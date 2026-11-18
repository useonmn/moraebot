//! Slack adapter over `FakeSlackTransport`: Socket Mode loop, acks, reconnects and sends.
//! Offline only. Waiting is on recorded state with a timeout; paused time is used only
//! where time is the behavior (backoff, silence, Retry-After).

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::{
    AttachmentRef, E_ADAPTER_DM, E_ADAPTER_SEND, E_ADAPTER_START, E_ADAPTER_UPLOAD_DENIED,
    InboundAttachment, MessengerAdapter,
};
use moraebot::files::{E_FILE_HOST, E_FILE_REMOTE, E_FILE_WRITE, Progress};
use moraebot::slack::{
    E_SLACK_API, E_SLACK_AUTH, E_SLACK_RATE, E_SLACK_UPLOAD, SlackAdapter, SlackTransport,
};
use moraebot::testing::{FAKE_FILE_ID, FakeSlackTransport, SlackFileCall, UploadStep};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout};

const WAIT: Duration = Duration::from_secs(5);
/// Bound for paused-time tests; the runtime jumps over sleeps, so this costs nothing.
const PAUSED_WAIT: Duration = Duration::from_secs(3600);

async fn within<T>(bound: Duration, future: impl Future<Output = T>) -> T {
    timeout(bound, future).await.expect("timed out")
}

fn setup() -> (Arc<FakeSlackTransport>, SlackAdapter) {
    let fake = Arc::new(FakeSlackTransport::new());
    let adapter = SlackAdapter::with_transport(fake.clone(), "U111".into(), "C222".into());
    (fake, adapter)
}

fn hello() -> Value {
    json!({ "type": "hello", "num_connections": 1 })
}

fn message_frame(envelope_id: &str, user: &str, ts: &str) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "payload": {
            "type": "event_callback",
            "event": {
                "type": "message", "user": user, "channel": "C222",
                "channel_type": "channel", "ts": ts, "text": "hello"
            }
        }
    })
}

fn ack(envelope_id: &str) -> Value {
    json!({ "envelope_id": envelope_id })
}

fn acks(fake: &FakeSlackTransport) -> Vec<Value> {
    fake.record()
        .socket_texts
        .iter()
        .map(|text| serde_json::from_str(text).expect("ack is JSON"))
        .collect()
}

#[tokio::test]
async fn start_fails_on_bad_bot_token() {
    let (fake, adapter) = setup();
    fake.fail_auth();
    let err = adapter.start().await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_START);
    assert!(err.message.starts_with(E_SLACK_AUTH));
    let record = fake.record();
    assert_eq!(record.auth_tests, 1);
    assert_eq!(record.opens, 0);
    assert!(record.connected_urls.is_empty());
}

#[tokio::test]
async fn event_is_acked_before_delivery() {
    let (fake, adapter) = setup();
    let mut rx = adapter.start().await.unwrap();
    let session = within(WAIT, fake.session(1)).await;
    session.frame(message_frame("e1", "U111", "1.1"));
    let event = within(WAIT, rx.recv()).await.expect("event delivered");
    assert_eq!(event.message_id, "C222:1.1");
    assert_eq!(acks(&fake), vec![ack("e1")]);
}

#[tokio::test]
async fn owner_message_delivered_other_dropped() {
    let (fake, adapter) = setup();
    let mut rx = adapter.start().await.unwrap();
    let session = within(WAIT, fake.session(1)).await;
    let mut stranger_dm = message_frame("e1", "U999", "1.1");
    stranger_dm["payload"]["event"]["channel"] = json!("D444");
    stranger_dm["payload"]["event"]["channel_type"] = json!("im");
    let mut other_channel = message_frame("e2", "U111", "2.2");
    other_channel["payload"]["event"]["channel"] = json!("C777");
    session.frame(stranger_dm);
    session.frame(other_channel);
    session.frame(message_frame("e3", "U111", "3.3"));
    let event = within(WAIT, rx.recv()).await.expect("event delivered");
    assert_eq!(event.author_id, "U111");
    assert_eq!(event.message_id, "C222:3.3");
    within(
        WAIT,
        fake.wait_until(|record| record.socket_texts.len() == 3),
    )
    .await;
    assert_eq!(acks(&fake), vec![ack("e1"), ack("e2"), ack("e3")]);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn redelivered_envelope_yields_same_message_id() {
    let (fake, adapter) = setup();
    let mut rx = adapter.start().await.unwrap();
    let session = within(WAIT, fake.session(1)).await;
    session.frame(message_frame("e1", "U111", "1.1"));
    session.frame(message_frame("e1-retry", "U111", "1.1"));
    let first = within(WAIT, rx.recv()).await.expect("first delivery");
    let second = within(WAIT, rx.recv()).await.expect("second delivery");
    assert_eq!(first.message_id, "C222:1.1");
    assert_eq!(second.message_id, first.message_id);
    assert_eq!(acks(&fake), vec![ack("e1"), ack("e1-retry")]);
}

#[tokio::test]
async fn disconnect_reopens_with_new_url_and_bumps_reconnects() {
    let (fake, adapter) = setup();
    let mut reconnects = adapter.reconnects().expect("slack reports reconnects");
    let _rx = adapter.start().await.unwrap();
    let first = within(WAIT, fake.session(1)).await;
    first.frame(hello());
    within(WAIT, reconnects.wait_for(|count| *count == 1))
        .await
        .unwrap();
    first.frame(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    let second = within(WAIT, fake.session(2)).await;
    second.frame(hello());
    within(WAIT, reconnects.wait_for(|count| *count == 2))
        .await
        .unwrap();
    let record = fake.record();
    assert_eq!(record.opens, 2);
    assert_eq!(
        record.connected_urls,
        vec!["wss://fake.test/link/1", "wss://fake.test/link/2"]
    );
}

#[tokio::test(start_paused = true)]
async fn socket_error_backs_off_then_reconnects() {
    let (fake, adapter) = setup();
    let _rx = adapter.start().await.unwrap();
    let first = within(PAUSED_WAIT, fake.session(1)).await;
    first.frame(hello());
    let failed_at = Instant::now();
    first.error();
    let second = within(PAUSED_WAIT, fake.session(2)).await;
    let waited = failed_at.elapsed();
    assert!(waited >= Duration::from_secs(5) && waited < Duration::from_secs(10));
    second.frame(hello());
    let failed_at = Instant::now();
    second.close();
    within(PAUSED_WAIT, fake.session(3)).await;
    let waited = failed_at.elapsed();
    assert!(waited >= Duration::from_secs(10) && waited < Duration::from_secs(20));
    assert_eq!(fake.record().opens, 3);
}

#[tokio::test(start_paused = true)]
async fn silence_triggers_reconnect() {
    let (fake, adapter) = setup();
    let _rx = adapter.start().await.unwrap();
    let first = within(PAUSED_WAIT, fake.session(1)).await;
    first.frame(hello());
    within(PAUSED_WAIT, fake.wait_until(|record| record.opens == 1)).await;
    let quiet_since = Instant::now();
    within(PAUSED_WAIT, fake.session(2)).await;
    assert!(quiet_since.elapsed() >= Duration::from_secs(90));
    let record = fake.record();
    assert!(record.pings >= 2, "pings every 30 s while silent");
    assert_eq!(record.closed_sockets, 1);
}

#[tokio::test(start_paused = true)]
async fn link_disabled_waits_the_longest_backoff() {
    let (fake, adapter) = setup();
    let _rx = adapter.start().await.unwrap();
    let first = within(PAUSED_WAIT, fake.session(1)).await;
    let disabled_at = Instant::now();
    first.frame(json!({ "type": "disconnect", "reason": "link_disabled" }));
    within(PAUSED_WAIT, fake.session(2)).await;
    assert!(disabled_at.elapsed() >= Duration::from_secs(300));
}

#[tokio::test]
async fn send_thread_target_posts_thread_ts() {
    let (fake, adapter) = setup();
    let sent = adapter.send("C222:1.1", "hi").await.unwrap();
    assert_eq!(sent.message_id, "C222:1.0001");
    assert_eq!(
        fake.record().posts,
        vec![json!({ "channel": "C222", "text": "hi", "thread_ts": "1.1" })]
    );
}

#[tokio::test]
async fn send_plain_target_has_no_thread_ts() {
    let (fake, adapter) = setup();
    let sent = adapter.send("D333", "hi").await.unwrap();
    assert_eq!(sent.message_id, "D333:1.0001");
    assert_eq!(
        fake.record().posts,
        vec![json!({ "channel": "D333", "text": "hi" })]
    );
}

#[tokio::test]
async fn open_dm_targets_the_user_and_posts_without_thread_ts() {
    let (fake, adapter) = setup();
    let target = adapter.open_dm("U111").await.unwrap();
    assert_eq!(target, "U111");
    within(WAIT, adapter.send(&target, "hi")).await.unwrap();
    let posts = fake.record().posts;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["channel"], "U111");
    assert_eq!(posts[0]["text"], "hi");
    assert!(posts[0].get("thread_ts").is_none());
}

#[tokio::test]
async fn open_dm_refuses_a_bad_user_id() {
    let (fake, adapter) = setup();
    for user_id in ["", "U111:1.1"] {
        assert_eq!(
            adapter.open_dm(user_id).await.unwrap_err().code,
            E_ADAPTER_DM
        );
    }
    assert!(fake.record().posts.is_empty());
}

#[tokio::test]
async fn send_escapes_text() {
    let (fake, adapter) = setup();
    adapter.send("D333", "a < b & c > \"d\"").await.unwrap();
    assert_eq!(
        fake.record().posts[0]["text"],
        "a &lt; b &amp; c &gt; \"d\""
    );
}

#[tokio::test(start_paused = true)]
async fn rate_limited_then_posted() {
    let (fake, adapter) = setup();
    fake.rate_limit_next(Duration::from_secs(2));
    let before = Instant::now();
    let sent = adapter.send("C222:1.1", "hi").await.unwrap();
    assert!(before.elapsed() >= Duration::from_secs(2));
    assert_eq!(sent.message_id, "C222:2.0001");
    let posts = fake.record().posts;
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0], posts[1]);
}

#[tokio::test(start_paused = true)]
async fn rate_limited_three_times_is_send_error() {
    let (fake, adapter) = setup();
    for _ in 0..3 {
        fake.rate_limit_next(Duration::from_secs(1));
    }
    let err = adapter.send("C222:1.1", "hi").await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_SEND);
    assert!(err.message.starts_with(E_SLACK_RATE));
    assert_eq!(fake.record().posts.len(), 3);
}

#[tokio::test]
async fn api_error_is_send_error() {
    let (fake, adapter) = setup();
    fake.fail_next_post();
    let err = adapter.send("C222", "hi").await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_SEND);
    assert!(err.message.starts_with(E_SLACK_API));
    assert_eq!(fake.record().posts.len(), 1);
}

#[tokio::test]
async fn create_thread_returns_message_id_without_api_call() {
    let (fake, adapter) = setup();
    let thread = adapter
        .create_thread("C222", "C222:1.1", "task")
        .await
        .unwrap();
    assert_eq!(thread, "C222:1.1");
    assert_eq!(fake.record(), Default::default());
}

const DOWNLOAD_URL: &str = "https://files.slack.com/files-pri/T111-F111/download/notes.txt";
const THREAD_TS: &str = "1700000000.000100";

fn slack_file(reference: &str, mime: Option<&str>) -> InboundAttachment {
    InboundAttachment {
        name: "notes.txt".into(),
        size: 5,
        mime: mime.map(str::to_string),
        reference: AttachmentRef(reference.into()),
    }
}

fn temp_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn file_calls(fake: &FakeSlackTransport) -> Vec<SlackFileCall> {
    fake.record().file_calls
}

#[tokio::test]
async fn fetch_uses_download_and_writes_file() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("0-notes.txt");
    fake.set_download_bytes(DOWNLOAD_URL, b"hello".to_vec());
    let progress = Progress::new();
    let n = adapter
        .fetch_attachment(
            &slack_file(DOWNLOAD_URL, Some("text/plain")),
            &dest,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(n, 5);
    assert_eq!(progress.bytes(), 5);
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    assert_eq!(
        std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        file_calls(&fake),
        vec![SlackFileCall::Download {
            url_host: "files.slack.com".into(),
            dest,
            reject_html: true,
        }]
    );
}

#[tokio::test]
async fn fetch_remote_is_file_remote() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let err = adapter
        .fetch_attachment(
            &slack_file("", None),
            &dir.path().join("x"),
            &Progress::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, E_FILE_REMOTE);
    assert!(file_calls(&fake).is_empty());
}

#[tokio::test]
async fn slack_download_html_for_pdf_is_denied() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    for (name, mime) in [("a.pdf", "application/pdf"), ("b.html", "text/html")] {
        adapter
            .fetch_attachment(
                &slack_file(DOWNLOAD_URL, Some(mime)),
                &dir.path().join(name),
                &Progress::new(),
            )
            .await
            .unwrap();
    }
    let flags: Vec<bool> = file_calls(&fake)
        .into_iter()
        .map(|call| match call {
            SlackFileCall::Download { reject_html, .. } => reject_html,
            other => panic!("unexpected call {other:?}"),
        })
        .collect();
    assert_eq!(flags, vec![true, false]);
}

#[tokio::test]
async fn fake_download_without_parent_dir_is_write_error() {
    let fake = FakeSlackTransport::new();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-dir");
    let err = fake
        .download(DOWNLOAD_URL, &missing.join("f.txt"), true, &Progress::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, E_FILE_WRITE);
    assert!(!missing.exists(), "the fake never creates directories");
}

#[tokio::test]
async fn send_file_thread_target_completes_with_parent_ts() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = temp_file(dir.path(), "report.txt", b"0123456789");
    let sent = adapter
        .send_file(&format!("C222:{THREAD_TS}"), &path, "report.txt")
        .await
        .unwrap();
    assert_eq!(sent.message_id, FAKE_FILE_ID);
    assert_eq!(
        file_calls(&fake),
        vec![
            SlackFileCall::UploadUrl {
                filename: "report.txt".into(),
                length: 10,
            },
            SlackFileCall::UploadBytes {
                host: "files.slack.com".into(),
                len: 10,
            },
            SlackFileCall::CompleteUpload {
                file_id: FAKE_FILE_ID.into(),
                channel_id: "C222".into(),
                thread_ts: Some(THREAD_TS.into()),
            },
        ]
    );
    assert!(fake.record().posts.is_empty(), "no chat.postMessage");
}

#[tokio::test]
async fn send_file_dm_target_has_no_thread_ts() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = temp_file(dir.path(), "a.txt", b"abc");
    adapter.send_file("D333", &path, "a.txt").await.unwrap();
    assert_eq!(
        file_calls(&fake).last(),
        Some(&SlackFileCall::CompleteUpload {
            file_id: FAKE_FILE_ID.into(),
            channel_id: "D333".into(),
            thread_ts: None,
        })
    );
}

#[tokio::test]
async fn send_file_reads_file_and_uploads_once() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let size = 200 * 1024;
    let path = temp_file(dir.path(), "big.bin", &vec![7u8; size]);
    adapter
        .send_file("C222:1.1", &path, "big.bin")
        .await
        .unwrap();
    let calls = file_calls(&fake);
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[0],
        SlackFileCall::UploadUrl {
            filename: "big.bin".into(),
            length: size as u64,
        }
    );
    assert_eq!(
        calls[1],
        SlackFileCall::UploadBytes {
            host: "files.slack.com".into(),
            len: size as u64,
        }
    );
    assert!(matches!(calls[2], SlackFileCall::CompleteUpload { .. }));
}

#[tokio::test]
async fn upload_url_on_other_host_is_refused_before_bytes() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = temp_file(dir.path(), "a.txt", b"abc");
    for url in [
        "https://files.slack.com.evil.test/upload/v1/x",
        "http://files.slack.com/upload/v1/x",
        "https://u@files.slack.com/upload/v1/x",
    ] {
        fake.set_upload_url(url);
        let err = adapter.send_file("C222", &path, "a.txt").await.unwrap_err();
        assert!(
            err.code == E_SLACK_UPLOAD || err.code == E_FILE_HOST,
            "{url}: {}",
            err.code
        );
    }
    assert!(
        file_calls(&fake)
            .iter()
            .all(|call| matches!(call, SlackFileCall::UploadUrl { .. })),
        "no bytes and no complete after a bad upload URL"
    );
}

#[tokio::test]
async fn complete_failure_is_upload_error() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = temp_file(dir.path(), "a.txt", b"abc");
    fake.fail_next_upload_step(UploadStep::CompleteUpload, "internal_error");
    let err = adapter.send_file("C222", &path, "a.txt").await.unwrap_err();
    assert_eq!(err.code, E_SLACK_UPLOAD);
    fake.fail_next_upload_step(UploadStep::CompleteUpload, "not_in_channel");
    let err = adapter.send_file("C222", &path, "a.txt").await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);
    assert_eq!(file_calls(&fake).len(), 6, "all three steps ran twice");
}

#[tokio::test]
async fn missing_scope_upload_is_denied() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = temp_file(dir.path(), "a.txt", b"abc");
    fake.fail_next_upload_step(UploadStep::UploadUrl, "missing_scope");
    let err = adapter.send_file("C222", &path, "a.txt").await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);
    assert_eq!(
        file_calls(&fake),
        vec![SlackFileCall::UploadUrl {
            filename: "a.txt".into(),
            length: 3,
        }]
    );
}

#[tokio::test]
async fn upload_bytes_413_is_denied() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = temp_file(dir.path(), "a.txt", b"abc");
    fake.fail_next_upload_bytes_status(413);
    let err = adapter.send_file("C222", &path, "a.txt").await.unwrap_err();
    assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);
    let calls = file_calls(&fake);
    assert_eq!(calls.len(), 2);
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, SlackFileCall::CompleteUpload { .. }))
    );
}

#[tokio::test]
async fn send_file_unreadable_file_is_denied() {
    let (fake, adapter) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = temp_file(dir.path(), "secret.txt", b"abc");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let err = adapter
        .send_file("C222", &path, "secret.txt")
        .await
        .unwrap_err();
    assert_eq!(err.code, E_ADAPTER_UPLOAD_DENIED);
    let missing = adapter
        .send_file("C222", &dir.path().join("missing.txt"), "missing.txt")
        .await
        .unwrap_err();
    assert_eq!(missing.code, E_ADAPTER_UPLOAD_DENIED);
    assert!(file_calls(&fake).is_empty(), "no UploadUrl recorded");
}

#[tokio::test]
async fn shutdown_stops_socket_task() {
    let (fake, adapter) = setup();
    let mut reconnects = adapter.reconnects().expect("slack reports reconnects");
    let mut rx = adapter.start().await.unwrap();
    let session = within(WAIT, fake.session(1)).await;
    session.frame(hello());
    within(WAIT, reconnects.wait_for(|count| *count == 1))
        .await
        .unwrap();
    within(WAIT, adapter.shutdown()).await;
    assert_eq!(fake.record().closed_sockets, 1);
    assert!(within(WAIT, rx.recv()).await.is_none());
    assert_eq!(fake.record().opens, 1);
}
