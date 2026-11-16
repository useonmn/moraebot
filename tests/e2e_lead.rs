//! Offline end-to-end tests of the lead and its lanes through the library-level gateway:
//! `FakeAdapter` or the real `SlackAdapter` over `FakeSlackTransport`, with `fake-omp` as
//! the lead and as every lane. Nothing leaves the machine.

use std::fs::OpenOptions;
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::{InboundEvent, MessengerAdapter};
use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform, SlackConfig};
use moraebot::dispatch::{N_LANE_INTERRUPTED, N_UNSUPPORTED_INPUT, notice_text};
use moraebot::gateway::{Gateway, GatewayError, STORE_FILE};
use moraebot::lane::E_LANE_INTERRUPTED;
use moraebot::lead;
use moraebot::omp::key_slug;
use moraebot::slack::SlackAdapter;
use moraebot::store::{LaneState, State, Store};
use moraebot::testing::{Call, FakeAdapter, FakeSlackTransport};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const OWNER: &str = "111";
const CHANNEL: &str = "333";
const DM_CHANNEL: &str = "444";
const THREAD: &str = "222";
const SLACK_OWNER: &str = "U111";
const SLACK_CHANNEL: &str = "C222";
const BOUND: Duration = Duration::from_secs(30);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("finished within bound")
}

/// Streams the lines fake-omp appends to `FAKE_OMP_LOG`. The log is a FIFO that the tap
/// holds open for reading and writing, so a test waits for a line instead of polling.
/// The lead and every lane share it.
struct LogTap {
    path: PathBuf,
    lines: mpsc::UnboundedReceiver<String>,
    seen: Vec<String>,
    marks: usize,
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
            path: path.to_owned(),
            lines,
            seen: Vec::new(),
            marks: 0,
        }
    }

    async fn wait_for(&mut self, matches: impl Fn(&str) -> bool) {
        bounded(async {
            if self.seen.iter().any(|line| matches(line)) {
                return;
            }
            loop {
                let line = self.lines.recv().await.expect("log tap open");
                let found = matches(&line);
                self.seen.push(line);
                if found {
                    return;
                }
            }
        })
        .await;
    }

    async fn wait_for_prompts(&mut self, n: usize) {
        bounded(async {
            while self.prompts().len() < n {
                let line = self.lines.recv().await.expect("log tap open");
                self.seen.push(line);
            }
        })
        .await;
    }

    /// Takes in every line written so far: writes an end mark into the FIFO and waits
    /// for it. The FIFO keeps write order, so every earlier line arrives first. Call it
    /// once the children are reaped (the gateway has stopped).
    async fn drain(&mut self) {
        self.marks += 1;
        let mark = format!("TAP_END {}", self.marks);
        let mut fifo = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .expect("open fifo for writing");
        fifo.write_all(format!("{mark}\n").as_bytes())
            .expect("write end mark");
        drop(fifo);
        self.wait_for(|line| line == mark).await;
    }

    /// `(pid, is_lane)` of every fake-omp start, as logged.
    fn starts(&self) -> Vec<(u32, bool)> {
        self.seen
            .iter()
            .filter_map(|line| line.strip_prefix("START pid="))
            .map(|rest| {
                let (pid, argv) = rest.split_once(" argv=").expect("argv");
                let argv: Vec<String> = serde_json::from_str(argv).expect("argv is JSON");
                let is_lane = argv.iter().any(|arg| arg.contains("/lanes/"));
                (pid.parse().expect("pid"), is_lane)
            })
            .collect()
    }

    fn lead_pids(&self) -> Vec<u32> {
        self.starts()
            .into_iter()
            .filter(|(_, lane)| !lane)
            .map(|(pid, _)| pid)
            .collect()
    }

    fn lane_pids(&self) -> Vec<u32> {
        self.starts()
            .into_iter()
            .filter(|(_, lane)| *lane)
            .map(|(pid, _)| pid)
            .collect()
    }

    /// Every logged prompt frame, lead and lanes, in log order.
    fn prompts(&self) -> Vec<Value> {
        self.seen
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|frame| frame["type"] == "prompt")
            .collect()
    }

    fn assert_no_child_left(&self) {
        for (pid, _) in self.starts() {
            assert!(!process_exists(pid), "fake-omp {pid} is still alive");
        }
    }
}

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("run kill")
        .success()
}

/// The tag of a logged prompt frame, when its message starts with one.
fn frame_tag(frame: &Value) -> Option<lead::Tag> {
    lead::parse_tag(frame["message"].as_str().expect("prompt message"))
}

struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), GatewayError>>,
}

impl Running {
    fn start(gateway: Gateway) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(gateway.run(async {
            let _ = stopped.await;
        }));
        Self { stop, task }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        bounded(self.task)
            .await
            .expect("gateway task joined")
            .expect("clean stop");
    }
}

/// A temp moraebot home (never named `home`), a work folder and the fake-omp log tap.
struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: Config,
    tap: LogTap,
}

impl Rig {
    fn discord() -> Self {
        Self::new(|home| {
            let token_file = home.join("secrets").join("discord.token");
            std::fs::write(&token_file, "TOKEN_SENTINEL_9f3a").expect("token file");
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
                .expect("token mode");
            (
                Platform::Discord,
                Some(DiscordConfig {
                    owner_id: OWNER.parse().unwrap(),
                    channel_id: CHANNEL.parse().unwrap(),
                    token_file,
                }),
                None,
            )
        })
    }

    fn slack() -> Self {
        Self::new(|home| {
            (
                Platform::Slack,
                None,
                Some(SlackConfig {
                    owner_id: SLACK_OWNER.into(),
                    channel_id: SLACK_CHANNEL.into(),
                    bot_token_file: home.join("secrets").join("slack-bot.token"),
                    app_token_file: home.join("secrets").join("slack-app.token"),
                }),
            )
        })
    }

    fn new(
        messenger: impl FnOnce(&Path) -> (Platform, Option<DiscordConfig>, Option<SlackConfig>),
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_owned();
        let home = root.join("mbhome");
        let work_dir = root.join("work");
        std::fs::create_dir_all(home.join("secrets")).expect("home");
        std::fs::create_dir_all(&work_dir).expect("work dir");
        let (platform, discord, slack) = messenger(&home);
        let config = Config {
            messenger: platform,
            work_dir,
            omp_path: PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            home,
            access_mode: AccessMode::OwnerOnly,
            discord,
            slack,
            omp: OmpConfig::default(),
        };
        let tap = LogTap::create(&root.join("fake-omp.log"));
        Self {
            _dir: dir,
            root,
            config,
            tap,
        }
    }

    /// A gateway whose lead and lanes get `FAKE_OMP_LOG` plus `vars`.
    fn gateway(&self, adapter: Arc<dyn MessengerAdapter>, vars: &[(&str, &str)]) -> Gateway {
        let mut env: Vec<(String, String)> = vec![(
            "FAKE_OMP_LOG".to_owned(),
            self.root
                .join("fake-omp.log")
                .to_string_lossy()
                .into_owned(),
        )];
        env.extend(
            vars.iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        Gateway::open(&self.config, adapter)
            .expect("open gateway")
            .with_omp(|omp| omp.with_extra_env(env))
    }

    /// The inbox after the gateway released it.
    fn store(&self) -> Store {
        Store::open(&self.config.home.join(STORE_FILE)).expect("reopen store")
    }
}

fn fake_adapter() -> (Arc<FakeAdapter>, Arc<dyn MessengerAdapter>) {
    let adapter = Arc::new(FakeAdapter::new());
    let dynamic: Arc<dyn MessengerAdapter> = adapter.clone();
    (adapter, dynamic)
}

fn owner_event(message_id: &str, text: &str) -> InboundEvent {
    InboundEvent {
        message_id: message_id.into(),
        author_id: OWNER.into(),
        author_is_bot: false,
        channel_id: CHANNEL.into(),
        thread_id: None,
        is_dm: false,
        text: text.into(),
        attachments: Vec::new(),
    }
}

fn dm(message_id: &str, text: &str) -> InboundEvent {
    InboundEvent {
        channel_id: DM_CHANNEL.into(),
        is_dm: true,
        ..owner_event(message_id, text)
    }
}

fn in_thread(thread: &str, message_id: &str, text: &str) -> InboundEvent {
    InboundEvent {
        thread_id: Some(thread.into()),
        ..owner_event(message_id, text)
    }
}

fn sends(adapter: &FakeAdapter) -> Vec<(String, String)> {
    adapter
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Send { channel, text } => Some((channel, text)),
            _ => None,
        })
        .collect()
}

fn lane_start_script(task: &str, done: &str) -> String {
    json!({"tool": "lane_start", "arguments": {"task": task, "done": done}}).to_string()
}

/// (a) One lead child answers a DM and a thread, each in its own place; a lane the lead
/// starts from the thread's turn comes back as a later lead turn answered in that thread.
#[tokio::test]
async fn one_lead_answers_each_conversation_and_the_lane_result_returns_to_its_thread() {
    let mut rig = Rig::discord();
    let (adapter, dynamic) = fake_adapter();
    let script = lane_start_script("summarize the notes", "file .");
    let running = Running::start(rig.gateway(
        dynamic,
        &[
            ("FAKE_OMP_MODE", "tools"),
            ("FAKE_OMP_TOOL_CALLS", &script),
            ("FAKE_OMP_LANE_MODE", "ok"),
        ],
    ));

    adapter
        .inject(dm("1300000000000000001", "hello lead"))
        .await;
    bounded(adapter.wait_for_send(1)).await;
    adapter
        .inject(in_thread(THREAD, "1300000000000000002", "run-tools please"))
        .await;
    bounded(adapter.wait_for_send(3)).await;
    running.stop().await;

    let sent = sends(&adapter);
    assert_eq!(
        sent[..2],
        [
            (DM_CHANNEL.to_owned(), "echo: hello lead".to_owned()),
            (THREAD.to_owned(), "echo: run-tools please".to_owned()),
        ],
        "each reply goes to the conversation its input came from"
    );
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[2].0, THREAD);
    assert!(
        sent[2].1.starts_with("echo: echo: summarize the notes"),
        "{sent:?}"
    );
    let store = rig.store();
    let lane = store.lane(1).expect("read lane").expect("lane exists");
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(lane.conversation_key, "fake:th:222");
    assert!(store.lane(2).expect("read lane").is_none(), "one lane");
    let result = store.get(3).expect("read row").expect("lane result row");
    assert_eq!(result.origin, "lane");
    assert_eq!(result.conversation_key, "fake:th:222");
    assert_eq!(result.state, State::Done);

    rig.tap.drain().await;
    assert_eq!(rig.tap.lead_pids().len(), 1, "one lead child in total");
    assert_eq!(rig.tap.lane_pids().len(), 1, "one lane child");
    let tags: Vec<lead::Tag> = rig.tap.prompts().iter().filter_map(frame_tag).collect();
    let conversations: Vec<&str> = tags.iter().map(|tag| tag.conversation.as_str()).collect();
    assert_eq!(conversations, ["fake:dm:444", "fake:th:222", "fake:th:222"]);
    assert_eq!(tags[0].message, Some(1));
    assert_eq!(tags[1].message, Some(2));
    assert_eq!(tags[2].lane, Some(1));
    let untagged: Vec<Value> = rig
        .tap
        .prompts()
        .into_iter()
        .filter(|frame| frame_tag(frame).is_none())
        .map(|frame| frame["message"].clone())
        .collect();
    assert_eq!(untagged.len(), 1, "{untagged:?}");
    assert!(
        untagged[0]
            .as_str()
            .unwrap_or_default()
            .starts_with("summarize the notes"),
        "the lane got its task"
    );
    rig.tap.assert_no_child_left();
}

fn slack_message(envelope_id: &str, ts: &str, thread_ts: Option<&str>, text: &str) -> Value {
    let mut frame = json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "payload": {
            "type": "event_callback",
            "event": {
                "type": "message", "user": SLACK_OWNER, "channel": SLACK_CHANNEL,
                "channel_type": "channel", "ts": ts, "text": text
            }
        }
    });
    if let Some(thread_ts) = thread_ts {
        frame["payload"]["event"]["thread_ts"] = json!(thread_ts);
    }
    frame
}

/// (b) Slack: the lead answers in the message's thread, and a same-thread follow-up sent
/// during the turn is steered into it and answered in the same single post.
#[tokio::test]
async fn slack_follow_up_in_the_same_thread_is_steered_into_one_reply() {
    let mut rig = Rig::slack();
    let fake = Arc::new(FakeSlackTransport::new());
    let adapter: Arc<dyn MessengerAdapter> = Arc::new(SlackAdapter::with_transport(
        fake.clone(),
        SLACK_OWNER.into(),
        SLACK_CHANNEL.into(),
    ));
    let gateway = rig.gateway(
        adapter,
        &[
            ("FAKE_OMP_MODE", "steer"),
            ("FAKE_OMP_STEER_CASE", "merged"),
        ],
    );
    let mut turn = gateway.running_turn();
    let running = Running::start(gateway);
    let session = bounded(fake.session(1)).await;

    session.frame(slack_message("e1", "1.1", None, "hello"));
    let key = bounded(turn.wait_for(Option::is_some))
        .await
        .expect("turn watch open")
        .clone();
    assert_eq!(key.as_deref(), Some("slack:th:C222:1.1"));
    session.frame(slack_message("e2", "2.2", Some("1.1"), "again"));
    bounded(fake.wait_until(|record| !record.posts.is_empty())).await;
    running.stop().await;

    let posts = fake.record().posts;
    assert_eq!(posts.len(), 1, "one post answers both messages");
    assert_eq!(posts[0]["channel"], SLACK_CHANNEL);
    assert_eq!(posts[0]["thread_ts"], "1.1");
    let text = posts[0]["text"].as_str().expect("post text");
    assert!(text.contains("hello") && text.contains("again"), "{text}");
    let store = rig.store();
    let main = store.get(1).expect("read row").expect("main row");
    let steered = store.get(2).expect("read row").expect("steered row");
    assert_eq!(main.state, State::Done);
    assert_eq!(steered.state, State::Done);
    assert_eq!(steered.steered_into, Some(1));

    rig.tap.drain().await;
    let prompts = rig.tap.prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[1]["streamingBehavior"], "steer");
    assert_eq!(frame_tag(&prompts[1]).and_then(|tag| tag.message), Some(2));
    assert_eq!(rig.tap.lead_pids().len(), 1);
    rig.tap.assert_no_child_left();
}

/// The slice-3 tables, as slice 3 left them: the slice-2 schema plus its four inbox
/// columns.
const SLICE3_SCHEMA: &str = "
CREATE TABLE inbox(
    id INTEGER PRIMARY KEY,
    platform TEXT NOT NULL,
    message_id TEXT NOT NULL,
    conversation_key TEXT NOT NULL,
    author_id TEXT NOT NULL,
    text TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('queued','processing','reply_pending','done','failed','needs_check')),
    reply TEXT,
    notice_sent INTEGER NOT NULL DEFAULT 0,
    received_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    error TEXT,
    attachments TEXT,
    files_ready INTEGER NOT NULL DEFAULT 1,
    out_files TEXT,
    text_sent INTEGER NOT NULL DEFAULT 0,
    UNIQUE(platform, message_id)
);
CREATE TABLE conversations(
    conversation_key TEXT PRIMARY KEY,
    platform TEXT,
    channel_id TEXT NOT NULL,
    omp_session_id TEXT,
    created_at TEXT,
    last_activity_at TEXT
);
CREATE TABLE known_threads(
    platform TEXT,
    thread_id TEXT,
    PRIMARY KEY(platform, thread_id)
);
";

const OLD_THREAD: &str = "1200000000000000001";
const OLD_SESSION_ID: &str = "old-session-111";

fn names(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).expect("prepare");
    stmt.query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("names")
}

/// (c) A slice-3 inbox migrates in place and keeps serving; the old per-conversation
/// session folder and session id are neither used nor touched.
#[tokio::test]
async fn slice3_home_is_migrated_and_old_session_folder_is_untouched() {
    let mut rig = Rig::discord();
    let store_file = rig.config.home.join(STORE_FILE);
    let old_key = format!("fake:th:{OLD_THREAD}");
    {
        let conn = rusqlite::Connection::open(&store_file).expect("create slice-3 store");
        conn.execute_batch(SLICE3_SCHEMA).expect("slice-3 schema");
        conn.execute(
            "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state,
                               reply, notice_sent, received_at, updated_at)
             VALUES ('fake', ?1, ?2, '111', 'old question', 'done', 'old answer', 0, 't', 't')",
            [OLD_THREAD, old_key.as_str()],
        )
        .expect("slice-3 row");
        conn.execute(
            "INSERT INTO conversations(conversation_key, platform, channel_id, omp_session_id,
                                       created_at, last_activity_at)
             VALUES (?1, 'fake', ?2, ?3, 't', 't')",
            [old_key.as_str(), CHANNEL, OLD_SESSION_ID],
        )
        .expect("slice-3 conversation");
    }
    let old_session = rig
        .config
        .home
        .join("conversations")
        .join(key_slug(&old_key))
        .join("session");
    std::fs::create_dir_all(&old_session).expect("old session folder");
    let marker = old_session.join("marker");
    std::fs::write(&marker, "slice-3 session").expect("marker file");

    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(rig.gateway(dynamic, &[("FAKE_OMP_MODE", "ok")]));
    adapter
        .inject(in_thread(OLD_THREAD, "1300000000000000001", "hello again"))
        .await;
    bounded(adapter.wait_for_send(1)).await;
    running.stop().await;

    assert_eq!(
        sends(&adapter),
        vec![(OLD_THREAD.to_owned(), "echo: hello again".to_owned())]
    );
    {
        let conn = rusqlite::Connection::open(&store_file).expect("open store file");
        let columns = names(&conn, "SELECT name FROM pragma_table_info('inbox')");
        for column in ["origin", "steered_into"] {
            assert!(columns.iter().any(|c| c == column), "missing {column}");
        }
        let tables = names(&conn, "SELECT name FROM sqlite_master WHERE type = 'table'");
        for table in ["sessions", "lanes"] {
            assert!(tables.iter().any(|t| t == table), "missing {table}");
        }
    }
    let store = rig.store();
    let old = store.get(1).expect("read row").expect("old row");
    assert_eq!(old.state, State::Done);
    assert_eq!(old.origin, "owner");
    assert_eq!(old.reply.as_deref(), Some("old answer"));
    let new = store.get(2).expect("read row").expect("new row");
    assert_eq!(new.state, State::Done);
    assert!(
        store
            .session_id(lead::LEAD_SESSION)
            .expect("read session")
            .is_some(),
        "the lead session id is stored under its own name"
    );
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker still there"),
        "slice-3 session"
    );

    rig.tap.drain().await;
    assert!(
        rig.tap
            .seen
            .iter()
            .all(|line| !line.contains(OLD_SESSION_ID)),
        "the old per-conversation session id is never sent to omp"
    );
    assert!(
        rig.tap
            .seen
            .iter()
            .filter(|line| line.starts_with("START "))
            .all(|line| !line.contains("/conversations/")),
        "the old session folder is never passed to omp"
    );
    rig.tap.assert_no_child_left();
}

/// (d) Stopping the gateway while a lane runs leaves the lane `needs_check`; the next run
/// posts one `N_LANE_INTERRUPTED` in the lane's conversation and restarts nothing.
#[tokio::test]
async fn shutdown_while_a_lane_runs_marks_it_and_the_next_run_posts_one_notice() {
    let mut rig = Rig::discord();
    let (adapter, dynamic) = fake_adapter();
    let script = lane_start_script("long job", "file .");
    let running = Running::start(rig.gateway(
        dynamic,
        &[
            ("FAKE_OMP_MODE", "tools"),
            ("FAKE_OMP_TOOL_CALLS", &script),
            ("FAKE_OMP_LANE_MODE", "hang"),
        ],
    ));
    adapter
        .inject(in_thread(THREAD, "1300000000000000001", "run-tools please"))
        .await;
    bounded(adapter.wait_for_send(1)).await;
    // The lead's prompt, then the lane's: the lane is in its (hanging) turn.
    rig.tap.wait_for_prompts(2).await;
    running.stop().await;

    let store = rig.store();
    let lane = store.lane(1).expect("read lane").expect("lane exists");
    assert_eq!(lane.state, LaneState::NeedsCheck);
    assert_eq!(lane.error.as_deref(), Some(E_LANE_INTERRUPTED));
    assert!(!lane.notice_sent);
    assert_eq!(store.count().expect("count rows"), 1, "no lane result row");
    drop(store);
    rig.tap.drain().await;
    assert_eq!(rig.tap.lane_pids().len(), 1);
    rig.tap.assert_no_child_left();

    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(rig.gateway(dynamic, &[("FAKE_OMP_MODE", "ok")]));
    bounded(adapter.wait_for_send(1)).await;
    running.stop().await;

    assert_eq!(
        sends(&adapter),
        vec![(
            THREAD.to_owned(),
            notice_text(N_LANE_INTERRUPTED).to_owned()
        )]
    );
    let lane = rig
        .store()
        .lane(1)
        .expect("read lane")
        .expect("lane exists");
    assert!(lane.notice_sent);
    assert_eq!(lane.state, LaneState::NeedsCheck);
    rig.tap.drain().await;
    assert_eq!(rig.tap.prompts().len(), 2, "nothing was re-prompted");
    assert_eq!(rig.tap.lane_pids().len(), 1, "the lane was not restarted");
    rig.tap.assert_no_child_left();
}

/// (e) The production stop path with a steered row: the worker is aborted, so only the
/// gateway's final recovery marks the group. Both rows end `needs_check`, nothing is
/// posted to the conversation and the lead child is gone.
#[tokio::test]
async fn production_shutdown_with_a_steered_row_marks_the_group_and_reaps_the_lead() {
    let mut rig = Rig::discord();
    let (adapter, dynamic) = fake_adapter();
    let gateway = rig.gateway(
        dynamic,
        &[("FAKE_OMP_MODE", "steer"), ("FAKE_OMP_STEER_CASE", "hold")],
    );
    let mut turn = gateway.running_turn();
    let running = Running::start(gateway);

    adapter
        .inject(in_thread(OWNER, "1300000000000000001", "first"))
        .await;
    let key = bounded(turn.wait_for(Option::is_some))
        .await
        .expect("turn watch open")
        .clone();
    assert_eq!(key.as_deref(), Some("fake:th:111"));
    adapter
        .inject(in_thread(OWNER, "1300000000000000002", "second"))
        .await;
    // Events are handled in order, so this notice proves "second" was handled (steered).
    adapter.inject(dm("1300000000000000003", "/marker")).await;
    bounded(adapter.wait_for_send(1)).await;
    rig.tap.wait_for_prompts(2).await;
    running.stop().await;

    let store = rig.store();
    let first = store.get(1).expect("read row").expect("first row");
    let second = store.get(2).expect("read row").expect("second row");
    assert_eq!(first.state, State::NeedsCheck);
    assert_eq!(second.state, State::NeedsCheck);
    assert_eq!(second.steered_into, Some(1));
    assert_eq!(store.count().expect("count rows"), 2);
    assert_eq!(
        sends(&adapter),
        vec![(
            DM_CHANNEL.to_owned(),
            notice_text(N_UNSUPPORTED_INPUT).to_owned()
        )],
        "nothing was sent to the thread"
    );

    rig.tap.drain().await;
    let prompts = rig.tap.prompts();
    assert_eq!(prompts.len(), 2, "the first prompt and the steer only");
    assert_eq!(prompts[1]["streamingBehavior"], "steer");
    let leads = rig.tap.lead_pids();
    assert_eq!(leads.len(), 1);
    assert!(!process_exists(leads[0]), "the lead child is gone");
    rig.tap.assert_no_child_left();
}
