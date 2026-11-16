use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moraebot::access::DropReason;
use moraebot::adapter::{
    AdapterError, E_ADAPTER_THREAD, InboundEvent, MessengerAdapter, SendResult, TypingHandle,
};
use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig};
use moraebot::dispatch::{
    Dispatcher, E_DISPATCH_HELD, Handled, MAX_REPLY_MESSAGES, MESSAGE_LIMIT, N_FAILED, N_INTERRUPTED,
    N_REPLY_TRUNCATED, N_THREAD_FAILED, N_UNSUPPORTED_INPUT, notice_text, reply_messages,
};
use moraebot::omp::{E_CHILD_LOST, E_OPEN_SESSION, OmpManager};
use moraebot::store::{State, Store};
use moraebot::testing::{Call, FakeAdapter};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

const OWNER: &str = "111";
const STRANGER: &str = "222";
const CHANNEL: &str = "333";
const BOUND: Duration = Duration::from_secs(30);
const TOKEN_SENTINEL: &str = "TOKEN_SENTINEL_9f3a";

struct Harness {
    root: PathBuf,
    config: Config,
    store: Arc<Mutex<Store>>,
    adapter: Arc<FakeAdapter>,
    dispatcher: Arc<Dispatcher>,
}

impl Harness {
    fn open(root: &Path, mode: &str) -> Self {
        Self::open_with(root, mode, PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")))
    }

    /// Like [`Self::open`], but every `create_thread` call is recorded and then fails.
    fn open_failing_threads(root: &Path) -> Self {
        Self::build(root, "ok", PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")), true)
    }

    fn open_with(root: &Path, mode: &str, omp_path: PathBuf) -> Self {
        Self::build(root, mode, omp_path, false)
    }

    fn build(root: &Path, mode: &str, omp_path: PathBuf, fail_threads: bool) -> Self {
        let home = root.join("home");
        let work_dir = root.join("work");
        std::fs::create_dir_all(home.join("secrets")).expect("home");
        std::fs::create_dir_all(&work_dir).expect("work dir");
        let token_file = home.join("secrets").join("discord.token");
        std::fs::write(&token_file, TOKEN_SENTINEL).expect("token file");
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
            .expect("token mode");
        let config = Config {
            owner_id: OWNER.parse().unwrap(),
            token_file,
            work_dir,
            omp_path,
            home: home.clone(),
            access_mode: AccessMode::OwnerOnly,
            discord: DiscordConfig { channel_id: CHANNEL.parse().unwrap() },
            omp: OmpConfig::default(),
        };
        let store = Arc::new(Mutex::new(Store::open(&home.join("inbox.sqlite")).expect("store")));
        let omp = OmpManager::new(config.clone(), Arc::clone(&store)).with_extra_env([
            ("FAKE_OMP_MODE", mode.to_owned()),
            ("FAKE_OMP_LOG", root.join("fake-omp.log").to_string_lossy().into_owned()),
        ]);
        let adapter = Arc::new(FakeAdapter::new());
        let as_dyn: Arc<dyn MessengerAdapter> = if fail_threads {
            Arc::new(ThreadFailAdapter(Arc::clone(&adapter)))
        } else {
            adapter.clone()
        };
        let dispatcher = Arc::new(Dispatcher::new(&config, as_dyn, Arc::clone(&store), omp));
        Self { root: root.to_owned(), config, store, adapter, dispatcher }
    }

    async fn handle(&self, event: InboundEvent) -> Handled {
        bounded(self.dispatcher.handle_event(event)).await.expect("handle event")
    }

    async fn step(&self) -> bool {
        bounded(self.dispatcher.process_next()).await.expect("worker step")
    }

    async fn drain(&self) {
        while self.step().await {}
    }

    fn row(&self, id: i64) -> moraebot::store::InboxRow {
        self.store.lock().unwrap().get(id).unwrap().expect("row exists")
    }

    fn count(&self) -> i64 {
        self.store.lock().unwrap().count().unwrap()
    }

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("fake-omp.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn prompts(&self) -> Vec<Value> {
        self.log_lines()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|f| f["type"] == "prompt")
            .collect()
    }

    fn starts(&self) -> usize {
        self.log_lines().iter().filter(|l| l.starts_with("START pid=")).count()
    }

    fn sends(&self) -> Vec<(String, String)> {
        self.adapter
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Send { channel, text } => Some((channel, text)),
                _ => None,
            })
            .collect()
    }

    fn threads_created(&self) -> usize {
        self.adapter
            .calls()
            .iter()
            .filter(|call| matches!(call, Call::CreateThread { .. }))
            .count()
    }

    async fn close(self) {
        self.dispatcher.shutdown().await;
    }

    /// Stores a channel message exactly as `handle_event` does, without opening its
    /// thread: the state left by a crash between admission and `create_thread`.
    fn admit_without_thread(&self, message_id: &str, text: &str) -> i64 {
        let admitted = self
            .store
            .lock()
            .unwrap()
            .admit("fake", message_id, &format!("fake:th:{message_id}"), CHANNEL, OWNER, text)
            .unwrap();
        match admitted {
            moraebot::store::Admitted::New(id) => id,
            other => panic!("expected New, got {other:?}"),
        }
    }
}

/// A [`FakeAdapter`] whose `create_thread` is recorded and then fails.
struct ThreadFailAdapter(Arc<FakeAdapter>);

#[async_trait]
impl MessengerAdapter for ThreadFailAdapter {
    fn platform(&self) -> &'static str {
        self.0.platform()
    }

    async fn start(&self) -> Result<mpsc::Receiver<InboundEvent>, AdapterError> {
        self.0.start().await
    }

    async fn send(&self, channel: &str, text: &str) -> Result<SendResult, AdapterError> {
        self.0.send(channel, text).await
    }

    async fn create_thread(
        &self,
        channel_id: &str,
        from_message_id: &str,
        name: &str,
    ) -> Result<String, AdapterError> {
        self.0.create_thread(channel_id, from_message_id, name).await?;
        Err(AdapterError { code: E_ADAPTER_THREAD, message: "injected thread failure".to_owned() })
    }

    async fn start_typing(&self, channel: &str) -> Result<TypingHandle, AdapterError> {
        self.0.start_typing(channel).await
    }

    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        self.0.reconnects()
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future).await.expect("finished within bound")
}

fn event(message_id: &str, author_id: &str, thread_id: Option<&str>, text: &str) -> InboundEvent {
    InboundEvent {
        message_id: message_id.into(),
        author_id: author_id.into(),
        author_is_bot: false,
        channel_id: CHANNEL.into(),
        thread_id: thread_id.map(str::to_owned),
        is_dm: false,
        text: text.into(),
    }
}

fn channel_msg(message_id: &str, text: &str) -> InboundEvent {
    event(message_id, OWNER, None, text)
}

fn queued(handled: Handled) -> i64 {
    match handled {
        Handled::Queued(id) => id,
        other => panic!("expected Queued, got {other:?}"),
    }
}

/// Shell stand-in for an omp child that dies with exit code 1 mid-turn: it forwards the
/// client's frames to fake-omp (mode `hang`), closes fake-omp's stdin right after the
/// prompt frame so it exits, then exits 1 itself. Written into the test's temp dir.
fn exit1_wrapper(root: &Path) -> PathBuf {
    let script = root.join("omp-exit1.sh");
    let fake = env!("CARGO_BIN_EXE_fake-omp");
    let body = format!(
        "#!/bin/sh\n\
         fifo=\"$(dirname \"$0\")/omp-exit1.fifo\"\n\
         rm -f \"$fifo\" && mkfifo \"$fifo\" || exit 2\n\
         '{fake}' \"$@\" < \"$fifo\" &\n\
         child=$!\n\
         exec 3>\"$fifo\"\n\
         while IFS= read -r line; do\n\
           printf '%s\\n' \"$line\" >&3\n\
           case \"$line\" in *'\"type\":\"prompt\"'*) break ;; esac\n\
         done\n\
         exec 3>&-\n\
         wait \"$child\"\n\
         exit 1\n"
    );
    std::fs::write(&script, body).expect("write wrapper");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    script
}

#[tokio::test]
async fn round_trip_creates_thread_and_replies() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let mid = "1300000000000000010";
    let id = queued(h.handle(channel_msg(mid, "hello")).await);
    assert!(h.step().await);

    assert_eq!(
        h.adapter.calls(),
        vec![
            Call::CreateThread {
                channel_id: CHANNEL.into(),
                from_message_id: mid.into(),
                name: "hello".into(),
            },
            Call::StartTyping { channel: mid.into() },
            Call::Send { channel: mid.into(), text: "echo: hello".into() },
        ]
    );
    let row = h.row(id);
    assert_eq!(row.state, State::Done);
    assert_eq!(row.conversation_key, format!("fake:th:{mid}"));
    assert_eq!(row.reply.as_deref(), Some("echo: hello"));
    assert!(!h.step().await);
    h.close().await;
}

#[tokio::test]
async fn crash_before_thread_creation_opens_thread_at_startup_then_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let mid = "1300000000000000140";
    let id = {
        let h = Harness::open(dir.path(), "ok");
        let id = h.admit_without_thread(mid, "hello");
        h.close().await;
        id
    };

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert_eq!(h.threads_created(), 1);
    assert!(h.prompts().is_empty(), "startup itself never prompts");
    h.drain().await;

    assert_eq!(
        h.adapter.calls(),
        vec![
            Call::CreateThread {
                channel_id: CHANNEL.into(),
                from_message_id: mid.into(),
                name: "hello".into(),
            },
            Call::StartTyping { channel: mid.into() },
            Call::Send { channel: mid.into(), text: "echo: hello".into() },
        ]
    );
    assert_eq!(h.prompts().len(), 1);
    assert_eq!(h.row(id).state, State::Done);
    h.close().await;
}

#[tokio::test]
async fn crash_before_thread_creation_and_failed_retry_marks_failed_with_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let mid = "1300000000000000141";
    let id = {
        let h = Harness::open(dir.path(), "ok");
        let id = h.admit_without_thread(mid, "hello");
        h.close().await;
        id
    };

    let h = Harness::open_failing_threads(dir.path());
    bounded(h.dispatcher.startup()).await.unwrap();
    h.drain().await;
    let row = h.row(id);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(E_ADAPTER_THREAD));
    assert!(row.notice_sent);
    assert_eq!(h.threads_created(), 1);
    assert_eq!(h.sends(), vec![(CHANNEL.to_owned(), notice_text(N_THREAD_FAILED).to_owned())]);
    assert!(h.prompts().is_empty());
    assert_eq!(h.starts(), 0);
    h.close().await;

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(!h.step().await);
    assert!(h.adapter.calls().is_empty(), "no second thread attempt and no second notice");
    assert!(h.prompts().is_empty());
    assert_eq!(h.row(id).state, State::Failed);
    h.close().await;
}

#[tokio::test]
async fn worker_never_prompts_a_thread_starter_without_its_thread() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_failing_threads(dir.path());
    let mid = "1300000000000000142";
    let id = h.admit_without_thread(mid, "hello");
    assert!(h.step().await);
    assert!(!h.step().await);

    let row = h.row(id);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(E_ADAPTER_THREAD));
    assert!(row.notice_sent);
    assert!(h.prompts().is_empty());
    assert_eq!(h.starts(), 0);
    assert_eq!(
        h.adapter.calls(),
        vec![
            Call::CreateThread {
                channel_id: CHANNEL.into(),
                from_message_id: mid.into(),
                name: "hello".into(),
            },
            Call::Send { channel: CHANNEL.into(), text: notice_text(N_THREAD_FAILED).into() },
        ]
    );
    h.close().await;
}

#[tokio::test]
async fn thread_creation_failure_on_arrival_marks_failed_with_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_failing_threads(dir.path());
    let handled = h.handle(channel_msg("1300000000000000143", "hello")).await;
    let Handled::ThreadFailed(id) = handled else { panic!("expected ThreadFailed, got {handled:?}") };
    assert!(!h.step().await);

    let row = h.row(id);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(E_ADAPTER_THREAD));
    assert!(row.notice_sent);
    assert!(h.prompts().is_empty());
    assert_eq!(h.sends(), vec![(CHANNEL.to_owned(), notice_text(N_THREAD_FAILED).to_owned())]);
    h.close().await;
}

#[tokio::test]
async fn run_loop_serves_injected_events() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let inbound = h.adapter.start().await.unwrap();
    let runner = tokio::spawn(Arc::clone(&h.dispatcher).run(inbound));
    h.adapter.inject(channel_msg("1300000000000000011", "one")).await;
    h.adapter.inject(channel_msg("1300000000000000012", "two")).await;
    bounded(h.adapter.wait_for_send(2)).await;
    let texts: Vec<String> = h.sends().into_iter().map(|(_, text)| text).collect();
    assert_eq!(texts, ["echo: one", "echo: two"]);
    runner.abort();
    let _ = runner.await;
    h.close().await;
}

#[tokio::test]
async fn message_in_thread_reuses_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let thread = "1300000000000000020";
    let first = queued(h.handle(channel_msg(thread, "first")).await);
    h.drain().await;
    let second = queued(h.handle(event("1300000000000000021", OWNER, Some(thread), "second")).await);
    h.drain().await;

    assert_eq!(h.threads_created(), 1);
    assert_eq!(h.starts(), 1, "same fake-omp child serves both messages");
    assert_eq!(h.row(first).conversation_key, h.row(second).conversation_key);
    assert_eq!(h.row(second).state, State::Done);
    assert_eq!(
        h.sends(),
        vec![
            (thread.to_owned(), "echo: first".to_owned()),
            (thread.to_owned(), "echo: second".to_owned()),
        ]
    );
    h.close().await;
}

#[tokio::test]
async fn non_owner_never_stored_or_sent() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let stranger = h.handle(event("1300000000000000030", STRANGER, None, "hello")).await;
    assert_eq!(stranger, Handled::Dropped(DropReason::NotOwner));
    let stranger_slash = h.handle(event("1300000000000000031", STRANGER, None, "/model x")).await;
    assert_eq!(stranger_slash, Handled::Dropped(DropReason::NotOwner));
    let mut bot = event("1300000000000000032", OWNER, None, "hello");
    bot.author_is_bot = true;
    assert_eq!(h.handle(bot).await, Handled::Dropped(DropReason::Bot));

    assert!(!h.step().await);
    assert_eq!(h.count(), 0);
    assert!(h.adapter.calls().is_empty());
    assert!(h.prompts().is_empty());
    assert_eq!(h.starts(), 0);
}

#[tokio::test]
async fn duplicate_message_id_processed_once() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let mid = "1300000000000000001";
    queued(h.handle(channel_msg(mid, "hello")).await);
    assert_eq!(h.handle(channel_msg(mid, "hello")).await, Handled::Duplicate);
    h.drain().await;
    assert_eq!(h.handle(channel_msg(mid, "hello")).await, Handled::Duplicate);
    assert!(!h.step().await);

    assert_eq!(h.count(), 1);
    assert_eq!(h.prompts().len(), 1);
    assert_eq!(h.threads_created(), 1);
    assert_eq!(h.sends().len(), 1);
    h.close().await;
}

#[tokio::test]
async fn omp_exit1_marks_needs_check_and_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_with(dir.path(), "hang", exit1_wrapper(dir.path()));
    let mid = "1300000000000000040";
    let id = queued(h.handle(channel_msg(mid, "hello")).await);
    assert!(h.step().await);

    let row = h.row(id);
    assert_eq!(row.state, State::NeedsCheck);
    assert_eq!(row.error.as_deref(), Some(E_CHILD_LOST));
    assert!(row.notice_sent);
    assert_eq!(h.prompts().len(), 1);
    assert_eq!(h.sends(), vec![(mid.to_owned(), notice_text(N_INTERRUPTED).to_owned())]);
    h.close().await;
}

#[tokio::test]
async fn queued_behind_interrupted_is_held_under_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_with(dir.path(), "hang", exit1_wrapper(dir.path()));
    let thread = "1300000000000000050";
    let first = queued(h.handle(channel_msg(thread, "first")).await);
    let second = queued(h.handle(event("1300000000000000051", OWNER, Some(thread), "second")).await);
    h.drain().await;

    assert_eq!(h.row(first).state, State::NeedsCheck);
    let held = h.row(second);
    assert_eq!(held.state, State::NeedsCheck);
    assert!(held.notice_sent);
    assert_eq!(h.prompts().len(), 1, "the held message never reaches omp");
    assert_eq!(h.sends(), vec![(thread.to_owned(), notice_text(N_INTERRUPTED).to_owned())]);
    h.close().await;
}

#[tokio::test]
async fn hold_behind_announced_interruption_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let thread = "1300000000000000055";
    let other = "1300000000000000056";
    let (first, other_id, second) = {
        let h = Harness::open_with(dir.path(), "hang", exit1_wrapper(dir.path()));
        let first = queued(h.handle(channel_msg(thread, "first")).await);
        let other_id = queued(h.handle(channel_msg(other, "other")).await);
        let second =
            queued(h.handle(event("1300000000000000057", OWNER, Some(thread), "second")).await);
        assert!(h.step().await);
        assert_eq!(h.row(first).state, State::NeedsCheck);
        assert_eq!(h.sends(), vec![(thread.to_owned(), notice_text(N_INTERRUPTED).to_owned())]);
        // The process stops while the other conversation's turn is in flight.
        h.store.lock().unwrap().mark(other_id, State::Processing, None).unwrap();
        h.close().await;
        (first, other_id, second)
    };

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    h.drain().await;

    let prompted: Vec<Value> = h.prompts().iter().map(|p| p["message"].clone()).collect();
    assert_eq!(prompted, ["first"], "the held message never reaches omp");
    let held = h.row(second);
    assert_eq!(held.state, State::NeedsCheck);
    assert_eq!(held.error.as_deref(), Some(E_DISPATCH_HELD));
    assert!(held.notice_sent);
    assert!(h.row(first).notice_sent);
    assert_eq!(h.row(other_id).state, State::NeedsCheck);
    assert_eq!(
        h.sends(),
        vec![(other.to_owned(), notice_text(N_INTERRUPTED).to_owned())],
        "no second notice for the already announced thread"
    );
    h.close().await;
}

#[tokio::test]
async fn restart_with_processing_row_sends_notice_and_no_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let mid = "1300000000000000060";
    let id = {
        let h = Harness::open(dir.path(), "ok");
        let id = queued(h.handle(channel_msg(mid, "hello")).await);
        h.store.lock().unwrap().mark(id, State::Processing, None).unwrap();
        id
    };

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert_eq!(h.sends(), vec![(mid.to_owned(), notice_text(N_INTERRUPTED).to_owned())]);
    assert!(!h.step().await);
    let row = h.row(id);
    assert_eq!(row.state, State::NeedsCheck);
    assert!(row.notice_sent);
    drop(h);

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(!h.step().await);
    assert!(h.sends().is_empty(), "the notice is posted exactly once across restarts");
    assert!(h.prompts().is_empty());
    assert_eq!(h.starts(), 0);
}

#[tokio::test]
async fn long_reply_split_into_chunks_all_under_2000() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let text = "word ".repeat(900);
    let id = queued(h.handle(channel_msg("1300000000000000070", &text)).await);
    h.drain().await;

    let reply = format!("echo: {text}");
    let chunks: Vec<String> = h.sends().into_iter().map(|(_, text)| text).collect();
    assert!(chunks.len() >= 3);
    assert!(chunks.iter().all(|c| c.chars().count() <= MESSAGE_LIMIT));
    assert_eq!(chunks.concat(), reply);
    assert_eq!(h.row(id).state, State::Done);
    h.close().await;
}

#[test]
fn reply_longer_than_ten_messages_is_cut_with_note() {
    let text = "x".repeat(MESSAGE_LIMIT * (MAX_REPLY_MESSAGES + 3));
    let chunks = reply_messages(&text);
    assert_eq!(chunks.len(), MAX_REPLY_MESSAGES);
    assert!(chunks.iter().all(|c| c.chars().count() <= MESSAGE_LIMIT));
    assert!(chunks.last().unwrap().ends_with(notice_text(N_REPLY_TRUNCATED)));
    assert_eq!(reply_messages("short"), ["short"]);
}

#[tokio::test]
async fn slash_message_not_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let handled = h.handle(channel_msg("1300000000000000080", "/model x")).await;
    assert_eq!(handled, Handled::Notice(N_UNSUPPORTED_INPUT));
    assert!(!h.step().await);
    assert!(h.prompts().is_empty());
    assert_eq!(h.count(), 0);
    assert_eq!(
        h.sends(),
        vec![(CHANNEL.to_owned(), notice_text(N_UNSUPPORTED_INPUT).to_owned())]
    );

    let empty = h.handle(channel_msg("1300000000000000081", "  ")).await;
    assert_eq!(empty, Handled::Notice(N_UNSUPPORTED_INPUT));
    assert_eq!(h.count(), 0);
    assert_eq!(h.threads_created(), 0);
}

#[tokio::test]
async fn open_session_failure_no_prompt_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "open_session_fail");
    let mid = "1300000000000000090";
    let id = queued(h.handle(channel_msg(mid, "hello")).await);
    assert!(h.step().await);

    let row = h.row(id);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(E_OPEN_SESSION));
    assert!(h.prompts().is_empty());
    assert_eq!(h.sends(), vec![(mid.to_owned(), notice_text(N_FAILED).to_owned())]);
    h.close().await;
}

#[tokio::test]
async fn send_failure_keeps_reply_pending_then_delivers_once() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let mid = "1300000000000000100";
    let id = queued(h.handle(channel_msg(mid, "hello")).await);
    h.adapter.fail_next_send();
    assert!(h.step().await);

    let row = h.row(id);
    assert_eq!(row.state, State::ReplyPending);
    assert_eq!(row.reply.as_deref(), Some("echo: hello"));

    assert_eq!(bounded(h.dispatcher.resend_pending()).await.unwrap(), 1);
    assert_eq!(h.row(id).state, State::Done);
    assert_eq!(bounded(h.dispatcher.resend_pending()).await.unwrap(), 0);
    assert!(!h.step().await);

    let sends = h.sends();
    assert_eq!(sends.len(), 2, "one failed attempt, one delivery");
    assert!(sends.iter().all(|(channel, text)| channel == mid && text == "echo: hello"));
    assert_eq!(h.prompts().len(), 1);
    h.close().await;
}

#[tokio::test]
async fn reply_pending_is_resent_once_after_adapter_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let inbound = h.adapter.start().await.unwrap();
    let runner = tokio::spawn(Arc::clone(&h.dispatcher).run(inbound));
    let mid = "1300000000000000115";
    h.adapter.fail_next_send();
    h.adapter.inject(channel_msg(mid, "hello")).await;
    bounded(h.adapter.wait_for_send(1)).await;
    // The reply is stored before the (failed) send, so the row is reply_pending now.
    assert_eq!(h.row(1).state, State::ReplyPending);

    h.adapter.reconnect();
    bounded(h.adapter.wait_for_send(2)).await;
    // A later message in the same thread is served after the resend finished, so the
    // first row is settled and the same omp child (no restart) is still in use.
    h.adapter.inject(event("1300000000000000116", OWNER, Some(mid), "second")).await;
    bounded(h.adapter.wait_for_send(3)).await;

    assert_eq!(h.row(1).state, State::Done);
    assert_eq!(
        h.sends(),
        vec![
            (mid.to_owned(), "echo: hello".to_owned()),
            (mid.to_owned(), "echo: hello".to_owned()),
            (mid.to_owned(), "echo: second".to_owned()),
        ],
        "one failed attempt, exactly one delivery after the reconnect"
    );
    let hello_prompts = h.prompts().iter().filter(|p| p["message"] == "hello").count();
    assert_eq!(hello_prompts, 1);
    assert_eq!(h.starts(), 1);
    runner.abort();
    let _ = runner.await;
    h.close().await;
}

#[tokio::test]
async fn reply_pending_is_resent_once_on_startup() {
    let dir = tempfile::tempdir().unwrap();
    let mid = "1300000000000000110";
    let id = {
        let h = Harness::open(dir.path(), "ok");
        let id = queued(h.handle(channel_msg(mid, "hello")).await);
        h.adapter.fail_next_send();
        assert!(h.step().await);
        assert_eq!(h.row(id).state, State::ReplyPending);
        h.close().await;
        id
    };

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert_eq!(h.sends(), vec![(mid.to_owned(), "echo: hello".to_owned())]);
    assert_eq!(h.row(id).state, State::Done);
    drop(h);

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(h.sends().is_empty());
    assert_eq!(h.prompts().len(), 1);
}

#[tokio::test]
async fn injection_payload_reaches_omp_byte_for_byte_and_never_runs() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let payload = "$(touch /tmp/moraebot-pwned-7f3a); touch /tmp/moraebot-pwned-7f3a-b";
    let mid = "1300000000000000120";
    queued(h.handle(channel_msg(mid, payload)).await);
    h.drain().await;

    let prompts = h.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["message"], payload);
    assert_eq!(h.sends(), vec![(mid.to_owned(), format!("echo: {payload}"))]);
    assert!(!Path::new("/tmp/moraebot-pwned-7f3a").exists());
    assert!(!Path::new("/tmp/moraebot-pwned-7f3a-b").exists());
    h.close().await;
}

#[tokio::test]
async fn token_sentinel_absent_from_logs_db_and_omp_io() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let guard = moraebot::logging::init(&h.config.home).expect("logging");
    queued(h.handle(channel_msg("1300000000000000130", "hello")).await);
    h.drain().await;
    assert_eq!(h.sends().len(), 1);
    let home = h.config.home.clone();
    let fake_log = dir.path().join("fake-omp.log");
    h.close().await;
    drop(guard);

    let mut checked = Vec::new();
    for sub in [home.clone(), home.join("logs")] {
        for entry in std::fs::read_dir(&sub).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_file() && (name.starts_with("inbox.sqlite") || sub.ends_with("logs")) {
                checked.push(path);
            }
        }
    }
    checked.push(fake_log);
    assert!(checked.iter().any(|p| p.to_string_lossy().contains("moraebot.log")));
    assert!(checked.iter().any(|p| p.ends_with("inbox.sqlite")));
    for path in checked {
        let bytes = std::fs::read(&path).unwrap();
        let found = bytes.windows(TOKEN_SENTINEL.len()).any(|w| w == TOKEN_SENTINEL.as_bytes());
        assert!(!found, "token sentinel leaked into {}", path.display());
    }
}
