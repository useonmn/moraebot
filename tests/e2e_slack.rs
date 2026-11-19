//! Offline end-to-end tests for Slack: the library-level gateway with the real
//! `SlackAdapter` over `FakeSlackTransport` and `fake-omp`. Nothing reaches the network.

use std::fs::OpenOptions;
use std::future::Future;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::MessengerAdapter;
use moraebot::config::{AccessMode, Config, OmpConfig, Platform, SlackConfig};
use moraebot::dispatch::{N_ONBOARDING_HELLO, notice_text};
use moraebot::gateway::{Gateway, GatewayError, STORE_FILE};
use moraebot::lead;
use moraebot::onboarding::{self, GreetTarget};
use moraebot::slack::{DEFAULT_HINTS, SlackAdapter, escape_text};
use moraebot::store::{State, Store};
use moraebot::testing::{FakeSlackSession, FakeSlackTransport};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const OWNER: &str = "U111";
const CHANNEL: &str = "C222";
const DM: &str = "D333";
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
    seen: Vec<String>,
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
        Self {
            lines,
            seen: Vec::new(),
        }
    }

    async fn wait_for_prompts(&mut self, n: usize) {
        bounded(async {
            while self.prompts() < n {
                let line = self.lines.recv().await.expect("log tap open");
                self.seen.push(line);
            }
        })
        .await;
    }

    fn pids(&self) -> Vec<u32> {
        self.seen
            .iter()
            .filter_map(|line| line.strip_prefix("START pid="))
            .map(|rest| {
                rest.split_whitespace()
                    .next()
                    .expect("pid")
                    .parse()
                    .expect("pid")
            })
            .collect()
    }

    fn prompts(&self) -> usize {
        self.seen.iter().filter(|line| is_prompt(line)).count()
    }

    /// The `message` of every prompt frame seen so far.
    fn prompt_messages(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|frame| frame["type"] == "prompt")
            .map(|frame| frame["message"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// The argv of every fake-omp start, as logged.
    fn argvs(&self) -> Vec<Vec<String>> {
        self.seen
            .iter()
            .filter_map(|line| line.split_once(" argv="))
            .map(|(_, argv)| serde_json::from_str(argv).expect("argv is JSON"))
            .collect()
    }
}

fn is_prompt(line: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|frame| frame["type"] == "prompt")
}

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("run kill")
        .success()
}

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: Config,
    fake: Arc<FakeSlackTransport>,
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
        let home = root.join("home");
        let work_dir = root.join("work");
        std::fs::create_dir_all(home.join("secrets")).expect("home");
        std::fs::create_dir_all(&work_dir).expect("work dir");
        let config = Config {
            messenger: Platform::Slack,
            work_dir,
            omp_path: PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            home: home.clone(),
            access_mode: AccessMode::OwnerOnly,
            discord: None,
            slack: Some(SlackConfig {
                owner_id: OWNER.into(),
                channel_id: CHANNEL.into(),
                bot_token_file: home.join("secrets").join("slack-bot.token"),
                app_token_file: home.join("secrets").join("slack-app.token"),
                access: Default::default(),
            }),
            omp: OmpConfig::default(),
        };
        let tap = LogTap::create(&root.join("fake-omp.log"));
        Self {
            _dir: dir,
            root,
            config,
            fake: Arc::new(FakeSlackTransport::new()),
            tap,
        }
    }

    /// Starts the gateway on a `SlackAdapter` over the fake transport and returns the
    /// first socket session once the adapter has connected it.
    async fn start(&self, mode: &str) -> (Running, FakeSlackSession) {
        let adapter: Arc<dyn MessengerAdapter> = Arc::new(SlackAdapter::with_transport(
            self.fake.clone(),
            OWNER.into(),
            CHANNEL.into(),
        ));
        let vars = [
            ("FAKE_OMP_MODE", mode.to_owned()),
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
        let session = bounded(self.fake.session(1)).await;
        (Running { stop, task }, session)
    }

    async fn wait_for_posts(&self, n: usize) {
        bounded(self.fake.wait_until(|record| record.posts.len() >= n)).await;
    }

    fn row_state(&self, id: i64) -> State {
        let store = Store::open(&self.config.home.join(STORE_FILE)).expect("reopen store");
        store.get(id).expect("read row").expect("row exists").state
    }

    fn row_count(&self) -> i64 {
        let store = Store::open(&self.config.home.join(STORE_FILE)).expect("reopen store");
        store.count().expect("count rows")
    }

    fn assert_no_child_left(&self) {
        for pid in self.tap.pids() {
            assert!(!process_exists(pid), "fake-omp {pid} is still alive");
        }
    }
}

fn message_frame(envelope_id: &str, user: &str, channel: &str, ts: &str, text: &str) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "payload": {
            "type": "event_callback",
            "event": {
                "type": "message", "user": user, "channel": channel,
                "channel_type": if channel.starts_with('D') { "im" } else { "channel" },
                "ts": ts, "text": text
            }
        }
    })
}

fn thread_reply_frame(envelope_id: &str, ts: &str, thread_ts: &str, text: &str) -> Value {
    let mut frame = message_frame(envelope_id, OWNER, CHANNEL, ts, text);
    frame["payload"]["event"]["thread_ts"] = json!(thread_ts);
    frame
}

fn post_texts(rig: &Rig) -> Vec<String> {
    rig.fake
        .record()
        .posts
        .iter()
        .map(|post| post["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn channel_message_gets_a_thread_reply_and_the_thread_continues() {
    let mut rig = Rig::new();
    let (running, session) = rig.start("ok").await;

    session.frame(message_frame("e1", OWNER, CHANNEL, "1.1", "hello"));
    rig.wait_for_posts(1).await;
    let first = rig.fake.record().posts[0].clone();
    assert_eq!(first["channel"], CHANNEL);
    assert_eq!(first["thread_ts"], "1.1");
    assert!(first["text"].as_str().unwrap().contains("echo: hello"));

    session.frame(thread_reply_frame("e2", "2.2", "1.1", "again"));
    rig.wait_for_posts(2).await;
    let second = rig.fake.record().posts[1].clone();
    assert_eq!(second["channel"], CHANNEL);
    assert_eq!(second["thread_ts"], "1.1", "same thread, no new one");
    assert!(second["text"].as_str().unwrap().contains("echo: again"));

    running.stop().await;
    assert_eq!(rig.row_state(1), State::Done);
    assert_eq!(rig.row_state(2), State::Done);
    rig.tap.wait_for_prompts(2).await;
    assert_eq!(rig.tap.pids().len(), 1, "one omp child serves the thread");
    let hints_flag = format!(
        "--append-system-prompt={}",
        lead::system_prompt(lead::DEFAULT_GUIDANCE, DEFAULT_HINTS)
    );
    assert!(
        rig.tap.argvs()[0].contains(&hints_flag),
        "omp gets the Slack default hints"
    );
    rig.assert_no_child_left();
}

#[tokio::test]
async fn owner_dm_is_answered_in_the_dm_without_thread_ts() {
    let mut rig = Rig::new();
    let (running, session) = rig.start("ok").await;

    session.frame(message_frame("e1", OWNER, DM, "5.5", "hi there"));
    rig.wait_for_posts(1).await;
    let post = rig.fake.record().posts[0].clone();
    assert_eq!(post["channel"], DM);
    assert!(
        post.get("thread_ts").is_none(),
        "DM replies are not threaded"
    );
    assert!(post["text"].as_str().unwrap().contains("echo: hi there"));

    running.stop().await;
    assert_eq!(rig.row_state(1), State::Done);
    rig.tap.wait_for_prompts(1).await;
    rig.assert_no_child_left();
}

#[tokio::test]
async fn slack_greeting_goes_to_the_owner_dm_and_the_reply_is_turn_1() {
    let mut rig = Rig::new();
    onboarding::request(&rig.config.home, GreetTarget::Dm).unwrap();
    let (running, session) = rig.start("ok").await;
    rig.wait_for_posts(1).await;
    let greeting = rig.fake.record().posts[0].clone();
    assert_eq!(greeting["channel"], OWNER);
    assert!(greeting.get("thread_ts").is_none());
    assert_eq!(
        greeting["text"],
        escape_text(notice_text(N_ONBOARDING_HELLO))
    );

    session.frame(message_frame("e1", OWNER, DM, "5.5", "Mina"));
    rig.wait_for_posts(2).await;
    let reply = rig.fake.record().posts[1].clone();
    assert_eq!(reply["channel"], DM);
    assert!(reply.get("thread_ts").is_none());

    rig.tap.wait_for_prompts(1).await;
    running.stop().await;
    assert_eq!(rig.row_state(1), State::Done);
    let messages = rig.tap.prompt_messages();
    assert_eq!(messages.len(), 1);
    let turn_1 = onboarding::notice_line(1);
    assert!(messages[0].lines().any(|line| line == turn_1));
    rig.assert_no_child_left();
}

#[tokio::test]
async fn other_users_and_bots_never_reach_the_store_or_omp() {
    let mut rig = Rig::new();
    let (running, session) = rig.start("ok").await;

    session.frame(message_frame("e1", "U999", CHANNEL, "1.1", "stranger"));
    session.frame(message_frame("e2", "U999", DM, "2.2", "stranger dm"));
    let mut from_bot = message_frame("e3", OWNER, CHANNEL, "3.3", "bot");
    from_bot["payload"]["event"]["bot_id"] = json!("B1");
    session.frame(from_bot);
    session.frame(message_frame("e4", OWNER, "C777", "4.4", "other channel"));
    // Frames are handled in order, so the owner's reply proves the four above were seen.
    session.frame(message_frame("e5", OWNER, CHANNEL, "5.5", "marker"));
    rig.wait_for_posts(1).await;

    running.stop().await;
    assert_eq!(rig.row_count(), 1, "only the owner's message was stored");
    assert_eq!(rig.fake.record().posts.len(), 1);
    rig.tap.wait_for_prompts(1).await;
    rig.assert_no_child_left();
}

#[tokio::test]
async fn a_redelivered_event_is_prompted_once() {
    let mut rig = Rig::new();
    let (running, session) = rig.start("ok").await;

    session.frame(message_frame("e1", OWNER, CHANNEL, "1.1", "once"));
    session.frame(message_frame("e1-retry", OWNER, CHANNEL, "1.1", "once"));
    session.frame(message_frame("e2", OWNER, CHANNEL, "2.2", "marker"));
    rig.wait_for_posts(2).await;

    running.stop().await;
    assert_eq!(rig.row_count(), 2);
    assert_eq!(
        rig.fake.record().posts.len(),
        2,
        "the retry was not answered again"
    );
    let texts = post_texts(&rig);
    assert!(texts[0].contains("echo: once"));
    assert!(texts[1].contains("echo: marker"));
    rig.tap.wait_for_prompts(2).await;
    rig.assert_no_child_left();
}

#[tokio::test]
async fn a_failed_post_is_re_sent_once_after_a_reconnect() {
    let mut rig = Rig::new();
    let (running, session) = rig.start("ok").await;

    rig.fake.fail_next_post();
    session.frame(message_frame("e1", OWNER, CHANNEL, "1.1", "hello"));
    rig.wait_for_posts(1).await;

    session.frame(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    let reconnected = bounded(rig.fake.session(2)).await;
    reconnected.frame(json!({ "type": "hello", "num_connections": 1 }));
    rig.wait_for_posts(2).await;

    running.stop().await;
    assert_eq!(rig.fake.record().posts.len(), 2, "re-sent exactly once");
    assert_eq!(rig.row_state(1), State::Done);
    let texts = post_texts(&rig);
    assert_eq!(texts[0], texts[1]);
    rig.tap.wait_for_prompts(1).await;
    rig.assert_no_child_left();
}

#[tokio::test]
async fn stopping_mid_turn_leaves_needs_check_and_no_child() {
    let mut rig = Rig::new();
    let (running, session) = rig.start("hang").await;

    session.frame(message_frame("e1", OWNER, CHANNEL, "1.1", "hello"));
    rig.tap.wait_for_prompts(1).await;

    running.stop().await;
    assert_eq!(rig.row_state(1), State::NeedsCheck);
    rig.assert_no_child_left();
}
