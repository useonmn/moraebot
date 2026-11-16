//! Slack adapter over `FakeSlackTransport`: Socket Mode loop, acks, reconnects and sends.
//! Offline only. Waiting is on recorded state with a timeout; paused time is used only
//! where time is the behavior (backoff, silence, Retry-After).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::{E_ADAPTER_SEND, E_ADAPTER_START, MessengerAdapter};
use moraebot::slack::{E_SLACK_API, E_SLACK_AUTH, E_SLACK_RATE, SlackAdapter};
use moraebot::testing::FakeSlackTransport;
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
