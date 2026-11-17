use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moraebot::access::{DropReason, E_ACCESS_NOT_OWNER};
use moraebot::adapter::{
    AdapterError, AttachmentRef, E_ADAPTER_THREAD, E_ADAPTER_UPLOAD, E_ADAPTER_UPLOAD_DENIED,
    InboundAttachment, InboundEvent, MessengerAdapter, SendResult, TypingHandle,
};
use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
use moraebot::dispatch::{
    Dispatcher, E_DISPATCH_HELD, Handled, MAX_REPLY_MESSAGES, N_FAILED, N_FILE_NOT_ALLOWED,
    N_FILE_NOT_RECEIVED, N_FILE_NOT_SENT, N_INTERRUPTED, N_LANE_INTERRUPTED, N_REPLY_TRUNCATED,
    N_THREAD_FAILED, N_UNSUPPORTED_INPUT, notice_text, reply_messages,
};
use moraebot::files::{self, OutFile, SavedFile};
use moraebot::gateway::Gateway;
use moraebot::hints;
use moraebot::lane::MAX_RUNNING_LANES;
use moraebot::lead::{self, LaneResult};
use moraebot::memory;
use moraebot::omp::{E_CHILD_LOST, E_OPEN_SESSION, OmpManager};
use moraebot::store::{Admitted, LaneStart, LaneState, State, Store};
use moraebot::testing::{Call, FakeAdapter};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const LIMIT: usize = 2000;
const OWNER: &str = "111";
const STRANGER: &str = "222";
const CHANNEL: &str = "333";
const BOUND: Duration = Duration::from_secs(30);
const TOKEN_SENTINEL: &str = "TOKEN_SENTINEL_9f3a";

/// The config of a test home under `root` (created, with its token file).
fn test_config(root: &Path, omp_path: PathBuf) -> Config {
    let home = root.join("home");
    let work_dir = root.join("work");
    std::fs::create_dir_all(home.join("secrets")).expect("home");
    std::fs::create_dir_all(&work_dir).expect("work dir");
    let token_file = home.join("secrets").join("discord.token");
    std::fs::write(&token_file, TOKEN_SENTINEL).expect("token file");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
        .expect("token mode");
    Config {
        messenger: Platform::Discord,
        work_dir,
        omp_path,
        home,
        access_mode: AccessMode::OwnerOnly,
        discord: Some(DiscordConfig {
            owner_id: OWNER.parse().unwrap(),
            channel_id: CHANNEL.parse().unwrap(),
            token_file,
        }),
        slack: None,
        omp: OmpConfig::default(),
    }
}

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
        Self::build(
            root,
            "ok",
            PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            true,
            Vec::new(),
        )
    }

    fn open_with(root: &Path, mode: &str, omp_path: PathBuf) -> Self {
        Self::build(root, mode, omp_path, false, Vec::new())
    }

    /// fake-omp in mode `send_file`: every turn calls the `send_file` host tool once per
    /// path, then replies `echo: <message>`.
    fn open_sending(root: &Path, paths: &[PathBuf]) -> Self {
        let script = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        Self::build(
            root,
            "send_file",
            PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            false,
            vec![("FAKE_OMP_SEND_FILE", script)],
        )
    }

    fn build(
        root: &Path,
        mode: &str,
        omp_path: PathBuf,
        fail_threads: bool,
        extra_env: Vec<(&'static str, String)>,
    ) -> Self {
        let config = test_config(root, omp_path);
        let home = config.home.clone();
        let store = Arc::new(Mutex::new(
            Store::open(&home.join("inbox.sqlite")).expect("store"),
        ));
        let mut env = vec![
            ("FAKE_OMP_MODE", mode.to_owned()),
            (
                "FAKE_OMP_LOG",
                root.join("fake-omp.log").to_string_lossy().into_owned(),
            ),
        ];
        env.extend(extra_env);
        let omp = OmpManager::new(config.clone(), Arc::clone(&store)).with_extra_env(env);
        let adapter = Arc::new(FakeAdapter::new());
        let as_dyn: Arc<dyn MessengerAdapter> = if fail_threads {
            Arc::new(ThreadFailAdapter(Arc::clone(&adapter)))
        } else {
            adapter.clone()
        };
        let dispatcher = Arc::new(Dispatcher::new(&config, as_dyn, Arc::clone(&store), omp));
        Self {
            root: root.to_owned(),
            config,
            store,
            adapter,
            dispatcher,
        }
    }

    async fn handle(&self, event: InboundEvent) -> Handled {
        bounded(self.dispatcher.handle_event(event))
            .await
            .expect("handle event")
    }

    async fn step(&self) -> bool {
        bounded(self.dispatcher.process_next())
            .await
            .expect("worker step")
    }

    async fn drain(&self) {
        while self.step().await {}
    }

    fn row(&self, id: i64) -> moraebot::store::InboxRow {
        self.store
            .lock()
            .unwrap()
            .get(id)
            .unwrap()
            .expect("row exists")
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

    /// Every prompt frame, its `message` turned into the body: the frame must start with a
    /// moraebot tag line, which is dropped.
    fn prompts(&self) -> Vec<Value> {
        self.log_lines()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|f| f["type"] == "prompt")
            .map(|mut frame| {
                let message = frame["message"].as_str().expect("prompt message");
                assert!(
                    lead::parse_tag(message).is_some(),
                    "prompt starts with a moraebot tag"
                );
                let body = message.split_once('\n').map_or("", |(_, body)| body);
                frame["message"] = Value::String(body.to_owned());
                frame
            })
            .collect()
    }

    fn starts(&self) -> usize {
        self.log_lines()
            .iter()
            .filter(|l| l.starts_with("START pid="))
            .count()
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
            .admit(
                "fake",
                message_id,
                &format!("fake:th:{message_id}"),
                CHANNEL,
                OWNER,
                text,
            )
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

    fn message_limit(&self) -> usize {
        self.0.message_limit()
    }

    fn behavior_hints(&self) -> &'static str {
        self.0.behavior_hints()
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
        self.0
            .create_thread(channel_id, from_message_id, name)
            .await?;
        Err(AdapterError {
            code: E_ADAPTER_THREAD,
            message: "injected thread failure".to_owned(),
        })
    }

    async fn start_typing(&self, channel: &str) -> Result<TypingHandle, AdapterError> {
        self.0.start_typing(channel).await
    }

    fn reconnects(&self) -> Option<watch::Receiver<u64>> {
        self.0.reconnects()
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("finished within bound")
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
        attachments: Vec::new(),
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
            Call::StartTyping {
                channel: mid.into()
            },
            Call::Send {
                channel: mid.into(),
                text: "echo: hello".into()
            },
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
            Call::StartTyping {
                channel: mid.into()
            },
            Call::Send {
                channel: mid.into(),
                text: "echo: hello".into()
            },
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
    assert_eq!(
        h.sends(),
        vec![(CHANNEL.to_owned(), notice_text(N_THREAD_FAILED).to_owned())]
    );
    assert!(h.prompts().is_empty());
    assert_eq!(h.starts(), 0);
    h.close().await;

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(!h.step().await);
    assert!(
        h.adapter.calls().is_empty(),
        "no second thread attempt and no second notice"
    );
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
            Call::Send {
                channel: CHANNEL.into(),
                text: notice_text(N_THREAD_FAILED).into()
            },
        ]
    );
    h.close().await;
}

#[tokio::test]
async fn thread_creation_failure_on_arrival_marks_failed_with_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_failing_threads(dir.path());
    let handled = h.handle(channel_msg("1300000000000000143", "hello")).await;
    let Handled::ThreadFailed(id) = handled else {
        panic!("expected ThreadFailed, got {handled:?}")
    };
    assert!(!h.step().await);

    let row = h.row(id);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(E_ADAPTER_THREAD));
    assert!(row.notice_sent);
    assert!(h.prompts().is_empty());
    assert_eq!(
        h.sends(),
        vec![(CHANNEL.to_owned(), notice_text(N_THREAD_FAILED).to_owned())]
    );
    h.close().await;
}

#[tokio::test]
async fn run_loop_serves_injected_events() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let inbound = h.adapter.start().await.unwrap();
    let runner = tokio::spawn(Arc::clone(&h.dispatcher).run(inbound));
    h.adapter
        .inject(channel_msg("1300000000000000011", "one"))
        .await;
    h.adapter
        .inject(channel_msg("1300000000000000012", "two"))
        .await;
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
    let second = queued(
        h.handle(event("1300000000000000021", OWNER, Some(thread), "second"))
            .await,
    );
    h.drain().await;

    assert_eq!(h.threads_created(), 1);
    assert_eq!(h.starts(), 1, "same fake-omp child serves both messages");
    assert_eq!(
        h.row(first).conversation_key,
        h.row(second).conversation_key
    );
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
    let stranger = h
        .handle(event("1300000000000000030", STRANGER, None, "hello"))
        .await;
    assert_eq!(stranger, Handled::Dropped(DropReason::NotOwner));
    let stranger_slash = h
        .handle(event("1300000000000000031", STRANGER, None, "/model x"))
        .await;
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
    assert_eq!(
        h.handle(channel_msg(mid, "hello")).await,
        Handled::Duplicate
    );
    h.drain().await;
    assert_eq!(
        h.handle(channel_msg(mid, "hello")).await,
        Handled::Duplicate
    );
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
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(N_INTERRUPTED).to_owned())]
    );
    h.close().await;
}

#[tokio::test]
async fn queued_behind_interrupted_is_held_under_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_with(dir.path(), "hang", exit1_wrapper(dir.path()));
    let thread = "1300000000000000050";
    let first = queued(h.handle(channel_msg(thread, "first")).await);
    let second = queued(
        h.handle(event("1300000000000000051", OWNER, Some(thread), "second"))
            .await,
    );
    h.drain().await;

    assert_eq!(h.row(first).state, State::NeedsCheck);
    let held = h.row(second);
    assert_eq!(held.state, State::NeedsCheck);
    assert!(held.notice_sent);
    assert_eq!(h.prompts().len(), 1, "the held message never reaches omp");
    assert_eq!(
        h.sends(),
        vec![(thread.to_owned(), notice_text(N_INTERRUPTED).to_owned())]
    );
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
        let second = queued(
            h.handle(event("1300000000000000057", OWNER, Some(thread), "second"))
                .await,
        );
        assert!(h.step().await);
        assert_eq!(h.row(first).state, State::NeedsCheck);
        assert_eq!(
            h.sends(),
            vec![(thread.to_owned(), notice_text(N_INTERRUPTED).to_owned())]
        );
        // The process stops while the other conversation's turn is in flight.
        h.store
            .lock()
            .unwrap()
            .mark(other_id, State::Processing, None)
            .unwrap();
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
        h.store
            .lock()
            .unwrap()
            .mark(id, State::Processing, None)
            .unwrap();
        id
    };

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(N_INTERRUPTED).to_owned())]
    );
    assert!(!h.step().await);
    let row = h.row(id);
    assert_eq!(row.state, State::NeedsCheck);
    assert!(row.notice_sent);
    drop(h);

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(!h.step().await);
    assert!(
        h.sends().is_empty(),
        "the notice is posted exactly once across restarts"
    );
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
    assert!(chunks.iter().all(|c| c.chars().count() <= LIMIT));
    assert_eq!(chunks.concat(), reply);
    assert_eq!(h.row(id).state, State::Done);
    h.close().await;
}

#[test]
fn reply_longer_than_ten_messages_is_cut_with_note() {
    let text = "x".repeat(LIMIT * (MAX_REPLY_MESSAGES + 3));
    let chunks = reply_messages(&text, LIMIT);
    assert_eq!(chunks.len(), MAX_REPLY_MESSAGES);
    assert!(chunks.iter().all(|c| c.chars().count() <= LIMIT));
    assert!(
        chunks
            .last()
            .unwrap()
            .ends_with(notice_text(N_REPLY_TRUNCATED))
    );
    assert_eq!(reply_messages("short", LIMIT), ["short"]);
}

#[tokio::test]
async fn slash_message_not_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let handled = h
        .handle(channel_msg("1300000000000000080", "/model x"))
        .await;
    assert_eq!(handled, Handled::Notice(N_UNSUPPORTED_INPUT));
    assert!(!h.step().await);
    assert!(h.prompts().is_empty());
    assert_eq!(h.count(), 0);
    assert_eq!(
        h.sends(),
        vec![(
            CHANNEL.to_owned(),
            notice_text(N_UNSUPPORTED_INPUT).to_owned()
        )]
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
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(N_FAILED).to_owned())]
    );
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
    assert!(
        sends
            .iter()
            .all(|(channel, text)| channel == mid && text == "echo: hello")
    );
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
    h.adapter
        .inject(event("1300000000000000116", OWNER, Some(mid), "second"))
        .await;
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
    let hello_prompts = h
        .prompts()
        .iter()
        .filter(|p| p["message"] == "hello")
        .count();
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
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), format!("echo: {payload}"))]
    );
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
    assert!(
        checked
            .iter()
            .any(|p| p.to_string_lossy().contains("moraebot.log"))
    );
    assert!(checked.iter().any(|p| p.ends_with("inbox.sqlite")));
    for path in checked {
        let bytes = std::fs::read(&path).unwrap();
        let found = bytes
            .windows(TOKEN_SENTINEL.len())
            .any(|w| w == TOKEN_SENTINEL.as_bytes());
        assert!(!found, "token sentinel leaked into {}", path.display());
    }
}

// ---- slice 3: inbound and outbound files ----

fn attachment(name: &str, reference: &str, size: u64) -> InboundAttachment {
    InboundAttachment {
        name: name.into(),
        size,
        mime: Some("text/plain".into()),
        reference: AttachmentRef(reference.into()),
    }
}

fn with_files(mut ev: InboundEvent, attachments: Vec<InboundAttachment>) -> InboundEvent {
    ev.attachments = attachments;
    ev
}

fn fetches(h: &Harness) -> Vec<PathBuf> {
    h.adapter
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::FetchAttachment { dest, .. } => Some(dest),
            _ => None,
        })
        .collect()
}

fn file_sends(h: &Harness) -> Vec<(String, u64)> {
    h.adapter
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::SendFile { name, bytes, .. } => Some((name, bytes)),
            _ => None,
        })
        .collect()
}

fn notices(h: &Harness, code: &str) -> usize {
    h.sends()
        .iter()
        .filter(|(_, text)| text == notice_text(code))
        .count()
}

fn out_files(h: &Harness, id: i64) -> Vec<OutFile> {
    serde_json::from_str(h.row(id).out_files.as_deref().expect("out_files stored")).unwrap()
}

/// The paths listed in a prompt's attached-files block.
fn listed_paths(message: &str) -> Vec<PathBuf> {
    message
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .filter_map(|l| l.split_once(" (name: ").map(|(p, _)| PathBuf::from(p)))
        .collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn handled_files_rejected(handled: Handled, notice: &str) {
    assert_eq!(
        handled,
        Handled::FilesRejected(match notice {
            N_FILE_NOT_ALLOWED => N_FILE_NOT_ALLOWED,
            _ => N_FILE_NOT_RECEIVED,
        })
    );
}

/// A file in the harness work folder; returns its canonical path.
fn work_file(h: &Harness, name: &str, bytes: &[u8]) -> PathBuf {
    let path = h.config.work_dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.canonicalize().unwrap()
}

/// Leaves a `reply_pending` row with its text delivered and one outbound file, as a
/// process that stopped during the file upload would (`attempting` when `in_flight`).
async fn seed_reply_with_file(h: &Harness, mid: &str, file: &Path, in_flight: bool) -> i64 {
    let id = queued(h.handle(channel_msg(mid, "hello")).await);
    let entry = OutFile {
        path: file.to_owned(),
        name: file.file_name().unwrap().to_string_lossy().into_owned(),
        sent: false,
        skipped: false,
        attempting: false,
        attempts: 0,
    };
    let json = serde_json::to_string(&vec![entry]).unwrap();
    let store = h.store.lock().unwrap();
    store.mark(id, State::Processing, None).unwrap();
    store.save_reply(id, "echo: hello", Some(&json)).unwrap();
    store.mark_text_sent(id).unwrap();
    if in_flight {
        store.mark_file_attempting(id, 0).unwrap();
    }
    id
}

#[tokio::test]
async fn attachment_downloaded_after_store_and_listed_in_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let mid = "1300000000000000200";
    let inbox = h.config.work_dir.join(files::INBOX_DIR);
    let slug_dir = inbox.join(format!("fake-th-{mid}"));
    let row_dir = slug_dir.join("1");
    assert!(!inbox.exists());
    h.adapter
        .set_attachment_bytes("ref-1", b"hello file".to_vec());
    let gate = h.adapter.hold_next_fetch();
    let ev = with_files(
        channel_msg(mid, "look at this"),
        vec![attachment("notes.txt", "ref-1", 10)],
    );
    let d = Arc::clone(&h.dispatcher);
    let join = tokio::spawn(async move { d.handle_event(ev).await });
    bounded(
        h.adapter
            .wait_for_call(|c| matches!(c, Call::FetchAttachment { .. })),
    )
    .await;
    // The fetch runs only once the row is stored, and the row waits for its files.
    assert_eq!(h.count(), 1);
    let waiting = h.row(1);
    assert_eq!(waiting.state, State::Queued);
    assert!(!waiting.files_ready);
    gate.notify_one();
    let id = queued(bounded(join).await.unwrap().unwrap());
    h.drain().await;

    for level in [&inbox, &slug_dir, &row_dir] {
        assert_eq!(mode(level), 0o700, "inbox level mode");
    }
    let prompts = h.prompts();
    assert_eq!(prompts.len(), 1);
    let message = prompts[0]["message"].as_str().unwrap().to_owned();
    assert!(message.starts_with("look at this\n\n[Attached files, saved on disk]\n"));
    let listed = listed_paths(&message);
    assert_eq!(listed, fetches(&h));
    assert_eq!(listed.len(), 1);
    assert!(listed[0].is_absolute());
    assert_eq!(listed[0].parent().unwrap(), row_dir);
    assert_eq!(std::fs::read(&listed[0]).unwrap(), b"hello file");
    assert_eq!(mode(&listed[0]), 0o600);
    assert_eq!(h.row(id).state, State::Done);
    h.close().await;
}

#[tokio::test]
async fn relative_work_dir_lists_absolute_paths() {
    std::fs::create_dir_all("target").unwrap();
    let tmp = tempfile::Builder::new()
        .prefix("s3-rel-")
        .tempdir_in("target")
        .unwrap();
    let cwd = std::env::current_dir().unwrap();
    let root = tmp.path().strip_prefix(&cwd).unwrap_or(tmp.path());
    assert!(root.is_relative());
    let h = Harness::open(root, "ok");
    assert!(h.config.work_dir.is_relative());
    h.adapter.set_attachment_bytes("ref-1", b"abc".to_vec());
    let mid = "1300000000000000201";
    queued(
        h.handle(with_files(
            channel_msg(mid, "hi"),
            vec![attachment("a.txt", "ref-1", 3)],
        ))
        .await,
    );
    h.drain().await;

    let message = h.prompts()[0]["message"].as_str().unwrap().to_owned();
    let listed = listed_paths(&message);
    assert_eq!(listed.len(), 1);
    assert!(listed[0].is_absolute());
    let expected_dir = cwd
        .join(root)
        .join("work")
        .join(files::INBOX_DIR)
        .join(format!("fake-th-{mid}"))
        .join("1");
    assert_eq!(listed[0].parent().unwrap(), expected_dir);
    assert!(listed[0].is_file());
    h.close().await;
}

#[tokio::test]
async fn inbox_dir_failure_fails_row_without_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    std::fs::write(h.config.work_dir.join(files::INBOX_DIR), b"not a folder").unwrap();
    let mid = "1300000000000000202";
    let handled = h
        .handle(with_files(
            channel_msg(mid, "hi"),
            vec![attachment("a.txt", "ref-1", 3)],
        ))
        .await;
    handled_files_rejected(handled, N_FILE_NOT_RECEIVED);
    assert!(!h.step().await);
    let row = h.row(1);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(files::E_FILE_WRITE));
    assert!(row.notice_sent);
    assert!(fetches(&h).is_empty());
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(N_FILE_NOT_RECEIVED).to_owned())]
    );
    assert!(h.prompts().is_empty());
    h.close().await;
}

#[tokio::test]
async fn attachment_only_message_runs() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    h.adapter.set_attachment_bytes("ref-1", b"img".to_vec());
    let mid = "1300000000000000203";
    let id = queued(
        h.handle(with_files(
            channel_msg(mid, ""),
            vec![attachment("photo.png", "ref-1", 3)],
        ))
        .await,
    );
    h.drain().await;
    let message = h.prompts()[0]["message"].as_str().unwrap().to_owned();
    assert!(message.starts_with("[Attached files, saved on disk]\n"));
    assert_eq!(listed_paths(&message).len(), 1);
    assert_eq!(h.row(id).state, State::Done);
    assert_eq!(notices(&h, N_UNSUPPORTED_INPUT), 0);
    h.close().await;
}

#[tokio::test]
async fn non_owner_attachment_never_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let handled = h
        .handle(with_files(
            event("1300000000000000204", STRANGER, None, "hi"),
            vec![attachment("a.txt", "ref-1", 3)],
        ))
        .await;
    assert_eq!(handled, Handled::Dropped(DropReason::NotOwner));
    assert_eq!(h.count(), 0);
    assert!(h.adapter.calls().is_empty());
    assert!(!h.config.work_dir.join(files::INBOX_DIR).exists());
}

#[tokio::test]
async fn too_many_files_rejected_without_store() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let many: Vec<InboundAttachment> = (0..=files::MAX_FILES)
        .map(|i| attachment(&format!("f{i}.txt"), &format!("ref-{i}"), 1))
        .collect();
    assert_eq!(many.len(), 11);
    let handled = h
        .handle(with_files(channel_msg("1300000000000000205", "hi"), many))
        .await;
    assert_eq!(handled, Handled::FilesRejected(N_FILE_NOT_RECEIVED));
    assert_eq!(h.count(), 0);
    assert!(fetches(&h).is_empty());
    assert_eq!(h.threads_created(), 0);
    assert_eq!(
        h.sends(),
        vec![(
            CHANNEL.to_owned(),
            notice_text(N_FILE_NOT_RECEIVED).to_owned()
        )]
    );
}

#[tokio::test]
async fn large_declared_size_is_not_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    h.adapter.set_attachment_bytes("ref-1", b"small".to_vec());
    let mid = "1300000000000000206";
    let id = queued(
        h.handle(with_files(
            channel_msg(mid, "big"),
            vec![attachment("big.bin", "ref-1", 5 * 1024 * 1024 * 1024)],
        ))
        .await,
    );
    h.drain().await;
    assert_eq!(fetches(&h).len(), 1);
    assert_eq!(h.prompts().len(), 1);
    assert_eq!(h.row(id).state, State::Done);
    assert_eq!(h.sends().len(), 1, "only the reply, no notice");
    h.close().await;
}

async fn fetch_fails_with(code: &'static str, notice: &'static str) {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    h.adapter.fail_next_fetch(code);
    let mid = "1300000000000000207";
    let handled = h
        .handle(with_files(
            channel_msg(mid, "hi"),
            vec![attachment("a.txt", "ref-1", 3)],
        ))
        .await;
    handled_files_rejected(handled, notice);
    assert!(!h.step().await);
    let row = h.row(1);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(code));
    assert!(row.notice_sent);
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(notice).to_owned())]
    );
    assert!(h.prompts().is_empty());
    h.close().await;
}

#[tokio::test]
async fn fetch_failure_fails_row_with_notice() {
    fetch_fails_with(files::E_FILE_FETCH, N_FILE_NOT_RECEIVED).await;
}

#[tokio::test]
async fn disk_full_fails_row_with_notice() {
    fetch_fails_with(files::E_FILE_WRITE, N_FILE_NOT_RECEIVED).await;
}

#[tokio::test]
async fn denied_download_posts_permission_notice() {
    fetch_fails_with(files::E_FILE_DENIED, N_FILE_NOT_ALLOWED).await;
}

#[tokio::test(start_paused = true)]
async fn fetch_timeout_fails_row_with_notice() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let _gate = h.adapter.hold_next_fetch();
    let mid = "1300000000000000208";
    let ev = with_files(
        channel_msg(mid, "hi"),
        vec![attachment("a.txt", "ref-1", 3)],
    );
    let start = tokio::time::Instant::now();
    let handled = tokio::time::timeout(files::STALL_TIMEOUT + BOUND, h.dispatcher.handle_event(ev))
        .await
        .expect("the stall watchdog ends the fetch")
        .unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed >= files::STALL_TIMEOUT, "elapsed {elapsed:?}");
    assert!(
        elapsed < files::STALL_TIMEOUT + BOUND,
        "elapsed {elapsed:?}"
    );
    assert_eq!(handled, Handled::FilesRejected(N_FILE_NOT_RECEIVED));
    let row = h.row(1);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(files::E_FILE_FETCH));
    assert_eq!(notices(&h, N_FILE_NOT_RECEIVED), 1);

    let slash = h.handle(channel_msg("1300000000000000209", "/x")).await;
    assert_eq!(slash, Handled::Notice(N_UNSUPPORTED_INPUT));
}

#[tokio::test(start_paused = true)]
async fn slow_progressing_download_succeeds_past_stall_window() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let mut feed = h.adapter.stream_next_fetch();
    let ev = with_files(
        channel_msg("1300000000000000210", "hi"),
        vec![attachment("slow.bin", "ref-1", 16)],
    );
    let start = tokio::time::Instant::now();
    let d = Arc::clone(&h.dispatcher);
    let join = tokio::spawn(async move { d.handle_event(ev).await });
    bounded(
        h.adapter
            .wait_for_call(|c| matches!(c, Call::FetchAttachment { .. })),
    )
    .await;
    for total in [4u64, 8, 12, 16] {
        tokio::time::advance(Duration::from_secs(100)).await;
        feed.chunks.send(vec![b'x'; 4]).await.unwrap();
        assert_eq!(bounded(feed.written.recv()).await, Some(total));
    }
    drop(feed.chunks);
    let id = queued(bounded(join).await.unwrap().unwrap());
    assert!(start.elapsed() >= Duration::from_secs(400));
    let row = h.row(id);
    assert_eq!(row.state, State::Queued);
    assert!(row.files_ready);
    let saved = fetches(&h);
    assert_eq!(std::fs::read(&saved[0]).unwrap().len(), 16);
    assert_eq!(notices(&h, N_FILE_NOT_RECEIVED), 0);
}

#[tokio::test]
async fn download_ending_after_hold_keeps_hold_and_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_with(dir.path(), "hang", exit1_wrapper(dir.path()));
    let thread = "1300000000000000211";
    let first = queued(h.handle(channel_msg(thread, "first")).await);
    h.adapter.fail_next_fetch(files::E_FILE_FETCH);
    let gate = h.adapter.hold_next_fetch();
    let b = with_files(
        event("1300000000000000212", OWNER, Some(thread), "second"),
        vec![attachment("a.txt", "ref-1", 3)],
    );
    let d = Arc::clone(&h.dispatcher);
    let join = tokio::spawn(async move { d.handle_event(b).await });
    bounded(
        h.adapter
            .wait_for_call(|c| matches!(c, Call::FetchAttachment { .. })),
    )
    .await;
    assert!(h.step().await);
    assert_eq!(h.row(first).state, State::NeedsCheck);
    gate.notify_one();
    bounded(join).await.unwrap().unwrap();

    let held = h.row(2);
    assert_eq!(held.state, State::NeedsCheck);
    assert_eq!(held.error.as_deref(), Some(E_DISPATCH_HELD));
    assert_eq!(
        h.sends(),
        vec![(thread.to_owned(), notice_text(N_INTERRUPTED).to_owned())]
    );
    assert_eq!(notices(&h, N_FILE_NOT_RECEIVED), 0);
    h.close().await;
}

#[tokio::test]
async fn slash_with_attachment_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let handled = h
        .handle(with_files(
            channel_msg("1300000000000000213", "/model x"),
            vec![attachment("a.txt", "ref-1", 3)],
        ))
        .await;
    assert_eq!(handled, Handled::Notice(N_UNSUPPORTED_INPUT));
    assert_eq!(h.count(), 0);
    assert!(fetches(&h).is_empty());
}

#[tokio::test]
async fn outbound_file_sent_after_text() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("work").join("report.txt");
    let h = Harness::open_sending(dir.path(), std::slice::from_ref(&report));
    work_file(&h, "report.txt", b"twelve bytes");
    let mid = "1300000000000000220";
    let id = queued(h.handle(channel_msg(mid, "make a report")).await);
    h.drain().await;

    let order: Vec<&str> = h
        .adapter
        .calls()
        .iter()
        .filter_map(|c| match c {
            Call::Send { .. } => Some("send"),
            Call::SendFile { .. } => Some("file"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["send", "file"]);
    assert_eq!(file_sends(&h), vec![("report.txt".to_owned(), 12)]);
    assert_eq!(h.row(id).state, State::Done);
    let entries = out_files(&h, id);
    assert!(entries[0].sent && !entries[0].skipped && !entries[0].attempting);
    h.close().await;
}

#[tokio::test]
async fn outbound_transient_error_notifies_once_and_resends_file() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("work").join("report.txt");
    let h = Harness::open_sending(dir.path(), std::slice::from_ref(&report));
    work_file(&h, "report.txt", b"data");
    h.adapter.fail_next_send_file(E_ADAPTER_UPLOAD);
    let id = queued(h.handle(channel_msg("1300000000000000221", "go")).await);
    h.drain().await;
    assert_eq!(h.row(id).state, State::ReplyPending);
    assert_eq!(notices(&h, N_FILE_NOT_SENT), 1);
    assert!(!out_files(&h, id)[0].attempting);

    bounded(h.dispatcher.resend_pending()).await.unwrap();
    assert_eq!(file_sends(&h).len(), 2);
    assert_eq!(h.sends().len(), 2, "one text send, one notice");
    assert_eq!(notices(&h, N_FILE_NOT_SENT), 1);
    assert_eq!(h.row(id).state, State::Done);
    assert!(out_files(&h, id)[0].sent);
    h.close().await;
}

#[tokio::test]
async fn outbound_permission_error_skips_file_with_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("work").join("report.txt");
    let h = Harness::open_sending(dir.path(), std::slice::from_ref(&report));
    work_file(&h, "report.txt", b"data");
    h.adapter.fail_next_send_file(E_ADAPTER_UPLOAD_DENIED);
    let id = queued(h.handle(channel_msg("1300000000000000222", "go")).await);
    h.drain().await;
    assert_eq!(h.row(id).state, State::Done);
    assert!(out_files(&h, id)[0].skipped);
    assert_eq!(notices(&h, N_FILE_NOT_SENT), 1);
    assert_eq!(h.sends().len(), 2, "one text send, one notice");
    assert_eq!(bounded(h.dispatcher.resend_pending()).await.unwrap(), 0);
    assert_eq!(file_sends(&h).len(), 1);
    h.close().await;
}

#[tokio::test]
async fn outbound_file_deleted_before_send_is_skipped_with_notice() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("work").join("report.txt");
    let h = Harness::open_sending(dir.path(), std::slice::from_ref(&report));
    let path = work_file(&h, "report.txt", b"data");
    h.adapter.fail_next_send_file(E_ADAPTER_UPLOAD);
    let id = queued(h.handle(channel_msg("1300000000000000223", "go")).await);
    h.drain().await;
    assert_eq!(h.row(id).state, State::ReplyPending);
    std::fs::remove_file(&path).unwrap();
    bounded(h.dispatcher.resend_pending()).await.unwrap();
    assert_eq!(file_sends(&h).len(), 1, "no upload of the deleted file");
    assert!(out_files(&h, id)[0].skipped);
    assert_eq!(notices(&h, N_FILE_NOT_SENT), 1);
    assert_eq!(h.row(id).state, State::Done);
    h.close().await;
}

#[tokio::test]
async fn outbound_unreadable_file_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let h = Harness::open_sending(dir.path(), &[work.join("one.txt"), work.join("two.txt")]);
    let one = work_file(&h, "one.txt", b"first");
    work_file(&h, "two.txt", b"second");
    h.adapter.fail_next_send_file(E_ADAPTER_UPLOAD);
    let id = queued(h.handle(channel_msg("1300000000000000224", "go")).await);
    h.drain().await;
    assert_eq!(h.row(id).state, State::ReplyPending);
    assert_eq!(notices(&h, N_FILE_NOT_SENT), 1);

    std::fs::set_permissions(&one, std::fs::Permissions::from_mode(0o000)).unwrap();
    h.adapter.reconnect();
    bounded(h.dispatcher.resend_pending()).await.unwrap();
    std::fs::set_permissions(&one, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        file_sends(&h),
        vec![("one.txt".to_owned(), 5), ("two.txt".to_owned(), 6)],
        "the unreadable file is not uploaded again"
    );
    let entries = out_files(&h, id);
    assert!(entries[0].skipped && entries[1].sent);
    assert_eq!(notices(&h, N_FILE_NOT_SENT), 1);
    assert_eq!(h.sends().len(), 2, "one text send, one notice");
    assert_eq!(h.row(id).state, State::Done);
    h.close().await;
}

#[tokio::test]
async fn startup_announces_incomplete_download() {
    let dir = tempfile::tempdir().unwrap();
    let mid = "1300000000000000225";
    let id = {
        let h = Harness::open(dir.path(), "ok");
        let mut store = h.store.lock().unwrap();
        let admitted = store
            .admit_with_files(
                "fake",
                mid,
                &format!("fake:th:{mid}"),
                CHANNEL,
                OWNER,
                "hi",
                Some(r#"[{"name":"a.txt","size":3,"mime":null}]"#),
            )
            .unwrap();
        store.remember_thread("fake", mid).unwrap();
        drop(store);
        match admitted {
            moraebot::store::Admitted::New(id) => id,
            other => panic!("expected New, got {other:?}"),
        }
    };

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(!h.step().await);
    let row = h.row(id);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(files::E_FILE_INCOMPLETE));
    assert!(row.notice_sent);
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(N_FILE_NOT_RECEIVED).to_owned())]
    );
    assert!(h.prompts().is_empty());
    drop(h);

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(h.sends().is_empty(), "the notice is posted once");
}

#[tokio::test]
async fn file_in_flight_at_crash_is_skipped_once_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mid = "1300000000000000226";
    let id = {
        let h = Harness::open(dir.path(), "ok");
        let file = work_file(&h, "report.txt", b"data");
        let id = seed_reply_with_file(&h, mid, &file, true).await;
        h.close().await;
        id
    };

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(file_sends(&h).is_empty());
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(N_FILE_NOT_SENT).to_owned())]
    );
    assert_eq!(h.row(id).state, State::Done);
    let entry = &out_files(&h, id)[0];
    assert!(entry.skipped && !entry.attempting);
    h.close().await;

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(h.adapter.calls().is_empty());
    h.close().await;
}

#[tokio::test(start_paused = true)]
async fn upload_timeout_skips_file_with_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let mid = "1300000000000000227";
    let id = {
        let h = Harness::open(dir.path(), "ok");
        let file = work_file(&h, "report.txt", b"data");
        let id = seed_reply_with_file(&h, mid, &file, false).await;
        h.close().await;
        id
    };

    let h = Harness::open(dir.path(), "ok");
    let _gate = h.adapter.hold_next_send_file();
    let start = tokio::time::Instant::now();
    tokio::time::timeout(files::UPLOAD_TIMEOUT + BOUND, h.dispatcher.startup())
        .await
        .expect("the upload timeout ends the call")
        .unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed >= files::UPLOAD_TIMEOUT, "elapsed {elapsed:?}");
    assert!(
        elapsed < files::UPLOAD_TIMEOUT + BOUND,
        "elapsed {elapsed:?}"
    );
    assert_eq!(file_sends(&h).len(), 1);
    assert_eq!(h.row(id).state, State::Done);
    let entry = &out_files(&h, id)[0];
    assert!(entry.skipped && !entry.attempting);
    assert_eq!(entry.attempts, 1);
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(N_FILE_NOT_SENT).to_owned())],
        "one notice, no text send"
    );
    assert_eq!(bounded(h.dispatcher.resend_pending()).await.unwrap(), 0);
    assert_eq!(file_sends(&h).len(), 1);
    h.close().await;

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(h.adapter.calls().is_empty());
    h.close().await;
}

#[tokio::test]
async fn transient_upload_failure_is_skipped_after_max_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let mid = "1300000000000000228";
    let id = {
        let h = Harness::open(dir.path(), "ok");
        let file = work_file(&h, "report.txt", b"data");
        let id = seed_reply_with_file(&h, mid, &file, false).await;
        h.close().await;
        id
    };

    let h = Harness::open(dir.path(), "ok");
    h.adapter.fail_next_send_file(E_ADAPTER_UPLOAD);
    bounded(h.dispatcher.startup()).await.unwrap();
    assert_eq!(file_sends(&h).len(), 1);
    assert_eq!(notices(&h, N_FILE_NOT_SENT), 1);
    assert_eq!(h.row(id).state, State::ReplyPending);
    for attempts in [2, 3] {
        h.adapter.fail_next_send_file(E_ADAPTER_UPLOAD);
        assert_eq!(bounded(h.dispatcher.resend_pending()).await.unwrap(), 0);
        assert_eq!(file_sends(&h).len(), attempts);
    }
    bounded(h.dispatcher.resend_pending()).await.unwrap();
    assert_eq!(file_sends(&h).len(), 3, "no fourth attempt");
    let entry = &out_files(&h, id)[0];
    assert!(entry.skipped && !entry.sent);
    assert_eq!(entry.attempts, files::MAX_UPLOAD_ATTEMPTS);
    assert_eq!(h.row(id).state, State::Done);
    assert_eq!(
        h.sends(),
        vec![(mid.to_owned(), notice_text(N_FILE_NOT_SENT).to_owned())],
        "one notice in total, no text send"
    );
    h.close().await;

    let h = Harness::open(dir.path(), "ok");
    bounded(h.dispatcher.startup()).await.unwrap();
    assert!(h.adapter.calls().is_empty());
    h.close().await;
}

#[tokio::test]
async fn next_message_waits_while_file_downloads() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let inbound = h.adapter.start().await.unwrap();
    let runner = tokio::spawn(Arc::clone(&h.dispatcher).run(inbound));
    let first = "1300000000000000230";
    let second = "1300000000000000231";
    let gate = h.adapter.hold_next_fetch();
    h.adapter
        .inject(with_files(
            channel_msg(first, "with file"),
            vec![attachment("a.txt", "ref-1", 3)],
        ))
        .await;
    bounded(
        h.adapter
            .wait_for_call(|c| matches!(c, Call::FetchAttachment { .. })),
    )
    .await;
    h.adapter.inject(channel_msg(second, "plain")).await;
    // `run` awaits one event at a time, so the second message is not admitted yet.
    assert_eq!(h.count(), 1);
    let second_thread = |c: &Call| matches!(c, Call::CreateThread { from_message_id, .. } if from_message_id == second);
    assert!(!h.adapter.calls().iter().any(second_thread));

    gate.notify_one();
    bounded(h.adapter.wait_for_call(second_thread)).await;
    bounded(h.adapter.wait_for_send(2)).await;
    let targets: Vec<String> = h.sends().into_iter().map(|(channel, _)| channel).collect();
    assert_eq!(targets, [first, second], "FIFO order unchanged");
    runner.abort();
    let _ = runner.await;
    h.close().await;
}

// --- One lead for every conversation, lane rows and steer (todo 7) ---

const THREAD_A: &str = "111";
const THREAD_B: &str = "222";
const DM_CHANNEL: &str = "D333";
const LANE_KEY: &str = "discord:th:222";

fn in_thread(message_id: &str, thread: &str, text: &str) -> InboundEvent {
    event(message_id, OWNER, Some(thread), text)
}

fn dm(message_id: &str, channel: &str, text: &str) -> InboundEvent {
    let mut ev = event(message_id, OWNER, None, text);
    ev.channel_id = channel.into();
    ev.is_dm = true;
    ev
}

fn steered(handled: Handled) -> i64 {
    match handled {
        Handled::Steered(id) => id,
        other => panic!("expected Steered, got {other:?}"),
    }
}

impl Harness {
    /// fake-omp in mode `steer`: the first prompt starts a run and waits for a steer,
    /// answered per `case` (`merged`, `separate`, `hold`).
    fn open_steer(root: &Path, case: &str) -> Self {
        Self::build(
            root,
            "steer",
            PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            false,
            vec![("FAKE_OMP_STEER_CASE", case.to_owned())],
        )
    }

    /// Runs one worker step on its own task: it returns once the turn's group is settled.
    fn spawn_step(&self) -> JoinHandle<bool> {
        let dispatcher = Arc::clone(&self.dispatcher);
        tokio::spawn(async move { dispatcher.process_next().await.expect("worker step") })
    }

    /// Waits until the lead reported the start of a turn for conversation `key`.
    async fn wait_turn(&self, key: &str) {
        let mut turn = self.dispatcher.running_turn();
        bounded(turn.wait_for(|running| running.as_deref() == Some(key)))
            .await
            .map(|_| ())
            .expect("omp manager alive");
    }

    /// Handles an event on its own task (for an event whose fetch is held).
    fn spawn_handle(&self, ev: InboundEvent) -> JoinHandle<Handled> {
        let dispatcher = Arc::clone(&self.dispatcher);
        tokio::spawn(async move { dispatcher.handle_event(ev).await.expect("handle event") })
    }

    async fn wait_for_fetch(&self) {
        bounded(
            self.adapter
                .wait_for_call(|c| matches!(c, Call::FetchAttachment { .. })),
        )
        .await;
    }

    /// The raw prompt frames, tag line included.
    fn raw_prompts(&self) -> Vec<Value> {
        self.log_lines()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|f| f["type"] == "prompt")
            .collect()
    }
}

fn behavior(frame: &Value) -> &str {
    frame["streamingBehavior"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn prompt_carries_conversation_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let id = queued(
        h.handle(dm("1300000000000000900", DM_CHANNEL, "hello"))
            .await,
    );
    h.drain().await;

    let frames = h.raw_prompts();
    assert_eq!(frames.len(), 1);
    let message = frames[0]["message"].as_str().unwrap();
    assert!(message.starts_with("[moraebot conversation="));
    let tag = lead::parse_tag(message).expect("tag line");
    assert_eq!(tag.conversation, h.row(id).conversation_key);
    assert_eq!(tag.message, Some(id));
    assert_eq!(tag.lane, None);
    assert_eq!(
        h.sends(),
        [(DM_CHANNEL.to_owned(), "echo: hello".to_owned())]
    );
    h.close().await;
}

#[tokio::test]
async fn replies_go_to_each_message_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    queued(
        h.handle(in_thread("1300000000000000901", THREAD_B, "in thread"))
            .await,
    );
    queued(
        h.handle(dm("1300000000000000902", DM_CHANNEL, "in dm"))
            .await,
    );
    h.drain().await;

    assert_eq!(
        h.sends(),
        [
            (THREAD_B.to_owned(), "echo: in thread".to_owned()),
            (DM_CHANNEL.to_owned(), "echo: in dm".to_owned()),
        ]
    );
    assert_eq!(h.starts(), 1, "one lead child serves both conversations");
    h.close().await;
}

#[tokio::test]
async fn lane_row_runs_without_owner_check_and_replies_to_lane_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let lane = match h
        .store
        .lock()
        .unwrap()
        .start_lane("toolu_1", LANE_KEY, "t", None, MAX_RUNNING_LANES)
        .unwrap()
    {
        LaneStart::New(id) => id,
        other => panic!("expected a new lane, got {other:?}"),
    };
    let input = lead::lane_result(LANE_KEY, lane, &LaneResult::Text("lane done"));
    assert!(
        h.store
            .lock()
            .unwrap()
            .finish_lane(lane, 1, &input, LaneState::Idle, None)
            .unwrap()
    );
    h.drain().await;

    let row = h.row(1);
    assert_eq!(row.origin, "lane");
    assert_eq!(row.state, State::Done);
    assert_eq!(
        h.sends(),
        [(THREAD_B.to_owned(), "echo: lane done".to_owned())]
    );
    let frames = h.raw_prompts();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0]["message"],
        input.as_str(),
        "the stored input is sent as is"
    );

    // An owner-origin row with a foreign author is still refused.
    let foreign = match h
        .store
        .lock()
        .unwrap()
        .admit(
            "fake",
            "1300000000000000903",
            LANE_KEY,
            CHANNEL,
            STRANGER,
            "x",
        )
        .unwrap()
    {
        Admitted::New(id) => id,
        other => panic!("expected New, got {other:?}"),
    };
    h.drain().await;
    let row = h.row(foreign);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.error.as_deref(), Some(E_ACCESS_NOT_OWNER));
    assert_eq!(
        h.raw_prompts().len(),
        1,
        "the foreign row never reaches omp"
    );
    h.close().await;
}

#[tokio::test]
async fn same_conversation_message_is_steered_into_running_turn() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_steer(dir.path(), "merged");
    let a = queued(
        h.handle(in_thread("1300000000000000910", THREAD_A, "A"))
            .await,
    );
    let step = h.spawn_step();
    h.wait_turn(&h.row(a).conversation_key).await;
    let b = steered(
        h.handle(in_thread("1300000000000000911", THREAD_A, "B"))
            .await,
    );
    assert!(bounded(step).await.unwrap());

    assert_eq!(h.sends(), [(THREAD_A.to_owned(), "echo: A + B".to_owned())]);
    assert_eq!(h.row(a).state, State::Done);
    let row_b = h.row(b);
    assert_eq!(row_b.state, State::Done);
    assert_eq!(row_b.reply, None);
    assert_eq!(row_b.steered_into, Some(a));
    let prompts = h.prompts();
    assert_eq!(prompts.len(), 2, "no second prompt for B");
    assert_eq!(behavior(&prompts[0]), "followUp");
    assert_eq!(behavior(&prompts[1]), "steer");
    assert_eq!(prompts[1]["message"], "B");
    assert!(!h.step().await, "nothing left to run");
    h.close().await;
}

#[tokio::test]
async fn separate_steer_answers_are_posted_once_each_in_one_reply() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_steer(dir.path(), "separate");
    let a = queued(
        h.handle(in_thread("1300000000000000920", THREAD_A, "A"))
            .await,
    );
    let step = h.spawn_step();
    h.wait_turn(&h.row(a).conversation_key).await;
    let b = steered(
        h.handle(in_thread("1300000000000000921", THREAD_A, "B"))
            .await,
    );
    assert!(bounded(step).await.unwrap());

    assert_eq!(
        h.sends(),
        [(THREAD_A.to_owned(), "echo: A\n\necho: B".to_owned())]
    );
    assert_eq!(h.row(a).state, State::Done);
    assert_eq!(h.row(b).state, State::Done);
    h.close().await;
}

#[tokio::test]
async fn other_conversation_message_waits_for_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_steer(dir.path(), "merged");
    let a = queued(
        h.handle(in_thread("1300000000000000930", THREAD_A, "A"))
            .await,
    );
    let step = h.spawn_step();
    h.wait_turn(&h.row(a).conversation_key).await;
    let c = queued(
        h.handle(in_thread("1300000000000000931", THREAD_B, "C"))
            .await,
    );
    // The steer releases A's turn; without it the turn would never end.
    steered(
        h.handle(in_thread("1300000000000000932", THREAD_A, "B"))
            .await,
    );
    assert!(bounded(step).await.unwrap());
    h.drain().await;
    bounded(h.adapter.wait_for_send(2)).await;

    let sends = h.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].0, THREAD_A);
    assert!(sends[0].1.contains("echo: A + B"));
    assert_eq!(sends[1], (THREAD_B.to_owned(), "echo: C".to_owned()));
    assert_eq!(h.row(c).state, State::Done);
    let prompts = h.prompts();
    let steer = prompts
        .iter()
        .position(|f| behavior(f) == "steer")
        .expect("steer frame");
    let c_frame = prompts
        .iter()
        .position(|f| f["message"] == "C")
        .expect("C's prompt frame");
    assert!(c_frame > steer, "C runs as a later turn");
    assert_eq!(behavior(&prompts[c_frame]), "followUp");
    h.close().await;
}

#[tokio::test]
async fn older_queued_message_of_conversation_blocks_steer() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_steer(dir.path(), "merged");
    let a = queued(
        h.handle(in_thread("1300000000000000940", THREAD_A, "A"))
            .await,
    );
    let step = h.spawn_step();
    h.wait_turn(&h.row(a).conversation_key).await;

    h.adapter.set_attachment_bytes("ref-b", b"bbb".to_vec());
    let gate = h.adapter.hold_next_fetch();
    let b_task = h.spawn_handle(with_files(
        in_thread("1300000000000000941", THREAD_A, "B"),
        vec![attachment("b.txt", "ref-b", 3)],
    ));
    h.wait_for_fetch().await;
    let waiting = h.row(2);
    assert_eq!(waiting.state, State::Queued);
    assert!(!waiting.files_ready);
    let c = queued(
        h.handle(in_thread("1300000000000000942", THREAD_A, "C"))
            .await,
    );
    gate.notify_one();
    let b = steered(bounded(b_task).await.unwrap());
    assert_eq!(b, 2);
    assert!(bounded(step).await.unwrap());
    h.drain().await;
    bounded(h.adapter.wait_for_send(2)).await;

    let sends = h.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].0, THREAD_A);
    assert!(
        sends[0]
            .1
            .starts_with("echo: A + B\n\n[Attached files, saved on disk]\n"),
        "A's and B's texts, A first"
    );
    assert_eq!(sends[1], (THREAD_A.to_owned(), "echo: C".to_owned()));
    assert_eq!(h.row(c).state, State::Done);

    let prompts = h.prompts();
    let steer = prompts
        .iter()
        .position(|f| behavior(f) == "steer")
        .expect("steer frame");
    let body = prompts[steer]["message"].as_str().unwrap();
    assert!(body.starts_with("B\n\n[Attached files, saved on disk]\n"));
    let saved: Vec<SavedFile> =
        serde_json::from_str(h.row(b).attachments.as_deref().expect("saved files")).unwrap();
    assert_eq!(saved.len(), 1);
    assert!(body.contains(saved[0].path.to_string_lossy().as_ref()));
    let c_frame = prompts
        .iter()
        .position(|f| f["message"] == "C")
        .expect("C's prompt frame");
    assert!(c_frame > steer, "C runs after the steer");
    assert_eq!(behavior(&prompts[c_frame]), "followUp");
    h.close().await;
}

#[tokio::test]
async fn group_needs_check_marks_every_member() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_steer(dir.path(), "hold");
    let a = queued(
        h.handle(in_thread("1300000000000000950", THREAD_A, "A"))
            .await,
    );
    let step = h.spawn_step();
    h.wait_turn(&h.row(a).conversation_key).await;
    let b = steered(
        h.handle(in_thread("1300000000000000951", THREAD_A, "B"))
            .await,
    );

    let gate = h.adapter.hold_next_fetch();
    let d_task = h.spawn_handle(with_files(
        in_thread("1300000000000000952", THREAD_A, "D"),
        vec![attachment("d.txt", "ref-d", 3)],
    ));
    h.wait_for_fetch().await;
    let d = 3;
    assert_eq!(h.row(d).text, "D");
    assert_eq!(h.row(d).state, State::Queued);

    // `hold` never ends the turn: only a shutdown that does not wait on it ends it.
    bounded(h.dispatcher.shutdown()).await;
    assert!(bounded(step).await.unwrap());
    bounded(h.adapter.wait_for_send(1)).await;

    for id in [a, b] {
        let row = h.row(id);
        assert_eq!(row.state, State::NeedsCheck);
        assert_eq!(row.error.as_deref(), Some(E_CHILD_LOST));
    }
    let row_d = h.row(d);
    assert_eq!(row_d.state, State::NeedsCheck);
    assert_eq!(row_d.error.as_deref(), Some(E_DISPATCH_HELD));
    assert_eq!(notices(&h, N_INTERRUPTED), 1);
    assert_eq!(
        h.sends(),
        [(THREAD_A.to_owned(), notice_text(N_INTERRUPTED).to_owned())]
    );

    gate.notify_one();
    bounded(d_task).await.unwrap();
    assert_eq!(h.row(d).state, State::NeedsCheck);
    assert_eq!(h.prompts().len(), 2, "A and B's steer only");
    h.close().await;
}

#[tokio::test]
async fn second_same_conversation_message_waits_for_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open_steer(dir.path(), "hold");
    let a = queued(
        h.handle(in_thread("1300000000000000960", THREAD_A, "A"))
            .await,
    );
    let step = h.spawn_step();
    h.wait_turn(&h.row(a).conversation_key).await;
    let b = steered(
        h.handle(in_thread("1300000000000000961", THREAD_A, "B"))
            .await,
    );
    let c = queued(
        h.handle(in_thread("1300000000000000962", THREAD_A, "C"))
            .await,
    );

    bounded(h.dispatcher.shutdown()).await;
    assert!(bounded(step).await.unwrap());
    bounded(h.adapter.wait_for_send(1)).await;

    for id in [a, b] {
        let row = h.row(id);
        assert_eq!(row.state, State::NeedsCheck);
        assert_eq!(row.error.as_deref(), Some(E_CHILD_LOST));
    }
    let row_c = h.row(c);
    assert_eq!(row_c.state, State::NeedsCheck);
    assert_eq!(row_c.error.as_deref(), Some(E_DISPATCH_HELD));
    assert_eq!(
        h.sends(),
        [(THREAD_A.to_owned(), notice_text(N_INTERRUPTED).to_owned())]
    );
    assert_eq!(h.prompts().len(), 2, "A and B's steer only");
    h.close().await;
}

#[tokio::test]
async fn startup_posts_lane_interrupted_once() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::open(dir.path(), "ok");
    let lane = {
        let mut store = h.store.lock().unwrap();
        let lane = match store
            .start_lane("toolu_1", LANE_KEY, "t", None, MAX_RUNNING_LANES)
            .unwrap()
        {
            LaneStart::New(id) => id,
            other => panic!("expected a new lane, got {other:?}"),
        };
        store.recover_on_start().unwrap();
        lane
    };
    bounded(h.dispatcher.startup()).await.unwrap();
    assert_eq!(
        h.sends(),
        [(
            THREAD_B.to_owned(),
            notice_text(N_LANE_INTERRUPTED).to_owned()
        )]
    );
    assert!(
        h.store
            .lock()
            .unwrap()
            .lane(lane)
            .unwrap()
            .unwrap()
            .notice_sent
    );

    bounded(h.dispatcher.startup()).await.unwrap();
    assert_eq!(notices(&h, N_LANE_INTERRUPTED), 1, "posted once");
    assert_eq!(h.raw_prompts().len(), 0, "nothing is re-prompted");
    h.close().await;
}

/// Runs a gateway on a fresh home under `root` (with `lead_override` as
/// `<home>/hints/lead.md`), serves one DM and returns the lead's
/// `--append-system-prompt=` argv elements.
async fn lead_system_prompt_flags(root: &Path, lead_override: Option<&str>) -> Vec<String> {
    let config = test_config(root, PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")));
    if let Some(text) = lead_override {
        let path = hints::override_path(&config.home, lead::GUIDANCE_NAME);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
    }
    let log = root.join("fake-omp.log");
    let adapter = Arc::new(FakeAdapter::new());
    let as_dyn: Arc<dyn MessengerAdapter> = adapter.clone();
    let vars = [
        ("FAKE_OMP_MODE", "ok".to_owned()),
        ("FAKE_OMP_LOG", log.to_string_lossy().into_owned()),
    ];
    let gateway = Gateway::open(&config, as_dyn)
        .expect("open gateway")
        .with_omp(|omp| omp.with_extra_env(vars));
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(gateway.run(async {
        let _ = stopped.await;
    }));
    adapter
        .inject(dm("1300000000000000970", DM_CHANNEL, "hello"))
        .await;
    bounded(adapter.wait_for_send(1)).await;
    let _ = stop.send(());
    bounded(task).await.unwrap().expect("clean stop");

    let log = std::fs::read_to_string(&log).expect("fake-omp log");
    let starts: Vec<&str> = log
        .lines()
        .filter_map(|l| l.strip_prefix("START pid="))
        .collect();
    assert_eq!(starts.len(), 1, "one lead child");
    let (_, argv) = starts[0].split_once(" argv=").expect("argv field");
    let argv: Vec<String> = serde_json::from_str(argv).expect("argv JSON");
    argv.into_iter()
        .filter(|a| a.starts_with("--append-system-prompt"))
        .collect()
}

#[tokio::test]
async fn gateway_appends_lead_guidance_then_messenger_hints() {
    let dir = tempfile::tempdir().unwrap();
    let hints_text = FakeAdapter::new().behavior_hints();
    let flag = "--append-system-prompt=";

    // Each flag ends with the memory block; compare the part before it.
    let before_memory = |flags: Vec<String>| -> Vec<String> {
        let marker = format!("\n\n{}", memory::BEGIN);
        flags
            .into_iter()
            .map(|flag| {
                let at = flag.find(&marker).expect("memory block in the flag");
                let block = &flag[at + 2..];
                assert!(block.starts_with(memory::BEGIN));
                assert!(block.ends_with(&format!("{}\n", memory::END)));
                flag[..at].to_owned()
            })
            .collect()
    };

    let first_line = lead::DEFAULT_GUIDANCE.lines().next().unwrap();
    let built_in =
        before_memory(lead_system_prompt_flags(&dir.path().join("built-in"), None).await);
    assert_eq!(built_in.len(), 1);
    assert!(built_in[0].starts_with(&format!("{flag}{first_line}")));
    assert!(built_in[0].ends_with(&format!("\n\n{hints_text}")));

    let custom = before_memory(
        lead_system_prompt_flags(&dir.path().join("custom"), Some("custom lead\n")).await,
    );
    assert_eq!(custom, [format!("{flag}custom lead\n\n{hints_text}")]);

    let empty = before_memory(lead_system_prompt_flags(&dir.path().join("empty"), Some("")).await);
    assert_eq!(empty, [format!("{flag}{hints_text}")]);
}
