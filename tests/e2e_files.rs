//! Offline end-to-end tests for files: the library-level gateway (`Gateway::open` + `run`)
//! with the real `SlackAdapter` over `FakeSlackTransport`, with `FakeAdapter` standing in for
//! a Discord-shaped messenger, and over a store file left by slice 2. The fakes never create
//! folders, so the private inbox levels prove the gateway path made them. Nothing reaches
//! the network.

use std::fs::OpenOptions;
use std::future::Future;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::{AttachmentRef, InboundAttachment, InboundEvent, MessengerAdapter};
use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform, SlackConfig};
use moraebot::dispatch::{N_FILE_NOT_SENT, notice_text};
use moraebot::files::INBOX_DIR;
use moraebot::gateway::{Gateway, GatewayError, STORE_FILE};
use moraebot::lead;
use moraebot::slack::{SlackAdapter, escape_text};
use moraebot::store::{State, Store};
use moraebot::testing::{
    Call, FAKE_FILE_ID, FakeAdapter, FakeSlackSession, FakeSlackTransport, SlackFileCall,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const SLACK_OWNER: &str = "U111";
const SLACK_CHANNEL: &str = "C222";
const DISCORD_OWNER: &str = "111";
const DISCORD_CHANNEL: &str = "222";
const DOWNLOAD_URL: &str = "https://files.slack.com/files-pri/T111-F222/download/report.txt";
const BLOCK_HEADER: &str = "[Attached files, saved on disk]";
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
            while self.prompts().len() < n {
                let line = self.lines.recv().await.expect("log tap open");
                self.seen.push(line);
            }
        })
        .await;
    }

    /// The body of every prompt frame fake-omp received, in order: its `message` must
    /// start with a moraebot tag line, which is dropped.
    fn prompts(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|frame| frame["type"] == "prompt")
            .map(|frame| {
                let message = frame["message"].as_str().unwrap_or_default();
                assert!(
                    lead::parse_tag(message).is_some(),
                    "prompt starts with a moraebot tag"
                );
                message
                    .split_once('\n')
                    .map_or("", |(_, body)| body)
                    .to_owned()
            })
            .collect()
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

    fn assert_no_child_left(&self) {
        for pid in self.pids() {
            let alive = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .expect("run kill")
                .success();
            assert!(!alive, "fake-omp {pid} is still alive");
        }
    }
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

/// A temp home and work folder, the config for one messenger, and the fake-omp log tap.
struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: Config,
    tap: LogTap,
}

impl Rig {
    fn new(messenger: Platform) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_owned();
        let home = root.join("home");
        let work_dir = root.join("work");
        std::fs::create_dir_all(home.join("secrets")).expect("home");
        std::fs::create_dir_all(&work_dir).expect("work dir");
        let (discord, slack) = match messenger {
            Platform::Slack => (
                None,
                Some(SlackConfig {
                    owner_id: SLACK_OWNER.into(),
                    channel_id: SLACK_CHANNEL.into(),
                    bot_token_file: home.join("secrets").join("slack-bot.token"),
                    app_token_file: home.join("secrets").join("slack-app.token"),
                    access: Default::default(),
                }),
            ),
            Platform::Discord => (
                Some(DiscordConfig {
                    owner_id: DISCORD_OWNER.parse().unwrap(),
                    channel_id: DISCORD_CHANNEL.parse().unwrap(),
                    token_file: home.join("secrets").join("discord.token"),
                    access: Default::default(),
                }),
                None,
            ),
        };
        let config = Config {
            messenger,
            work_dir,
            omp_path: PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            home,
            access_mode: AccessMode::OwnerOnly,
            discord,
            slack,
            omp: OmpConfig::default(),
            memory: moraebot::config::MemoryConfig::default(),
        };
        let tap = LogTap::create(&root.join("fake-omp.log"));
        Self {
            _dir: dir,
            root,
            config,
            tap,
        }
    }

    fn work_dir(&self) -> &Path {
        &self.config.work_dir
    }

    fn outbound_file(&self, bytes: &[u8]) -> PathBuf {
        let out = self.work_dir().join("out");
        std::fs::create_dir_all(&out).expect("out dir");
        let path = out.join("result.txt");
        std::fs::write(&path, bytes).expect("outbound file");
        path
    }

    /// Starts the gateway on `adapter` with fake-omp in `mode`; `send_file` lists the paths
    /// fake-omp passes to the `send_file` tool in every turn.
    fn start(
        &self,
        adapter: Arc<dyn MessengerAdapter>,
        mode: &str,
        send_file: Option<&Path>,
    ) -> Running {
        let mut vars = vec![
            ("FAKE_OMP_MODE", mode.to_owned()),
            (
                "FAKE_OMP_LOG",
                self.root
                    .join("fake-omp.log")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ];
        if let Some(path) = send_file {
            vars.push(("FAKE_OMP_SEND_FILE", path.to_string_lossy().into_owned()));
        }
        let gateway = Gateway::open(&self.config, adapter)
            .expect("open gateway")
            .with_omp(|omp| omp.with_extra_env(vars));
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(gateway.run(async {
            let _ = stopped.await;
        }));
        Running { stop, task }
    }

    fn store(&self) -> Store {
        Store::open(&self.config.home.join(STORE_FILE)).expect("reopen store")
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// Checks the received file and every inbox level the dispatcher created: the file sits at
/// `<work>/.moraebot-inbox/<slug>/<row>/<name>` with mode 600, each level with mode 700.
fn assert_saved_privately(work_dir: &Path, saved: &Path, bytes: &[u8]) {
    assert!(saved.is_absolute(), "listed path is absolute");
    assert_eq!(std::fs::read(saved).expect("saved file"), bytes);
    assert_eq!(mode_of(saved), 0o600);
    let inbox = work_dir.join(INBOX_DIR);
    let row_dir = saved.parent().expect("row folder");
    let slug_dir = row_dir.parent().expect("conversation folder");
    assert_eq!(slug_dir.parent(), Some(inbox.as_path()));
    for level in [&inbox, &slug_dir.to_path_buf(), &row_dir.to_path_buf()] {
        assert_eq!(mode_of(level), 0o700, "inbox level mode");
    }
}

/// The one saved path the attached-files block lists for `name` in a prompt message.
fn listed_path(message: &str, name: &str) -> PathBuf {
    let (_, block) = message
        .split_once(BLOCK_HEADER)
        .expect("prompt has the attached-files block");
    let line = block
        .lines()
        .find(|line| line.contains(&format!("(name: {name},")))
        .expect("block lists the file");
    let path = line
        .strip_prefix("- ")
        .and_then(|rest| rest.split(" (name: ").next())
        .expect("block line shape");
    PathBuf::from(path)
}

fn slack_adapter(fake: &Arc<FakeSlackTransport>) -> Arc<dyn MessengerAdapter> {
    Arc::new(SlackAdapter::with_transport(
        fake.clone(),
        SLACK_OWNER.into(),
        SLACK_CHANNEL.into(),
    ))
}

fn slack_message(envelope_id: &str, user: &str, ts: &str, text: &str) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "payload": {
            "type": "event_callback",
            "event": {
                "type": "message", "user": user, "channel": SLACK_CHANNEL,
                "channel_type": "channel", "ts": ts, "text": text
            }
        }
    })
}

fn file_share(envelope_id: &str, user: &str, ts: &str, text: &str) -> Value {
    let mut frame = slack_message(envelope_id, user, ts, text);
    let event = &mut frame["payload"]["event"];
    event["subtype"] = json!("file_share");
    event["files"] = json!([{
        "id": "F222",
        "name": "report.txt",
        "mimetype": "text/plain",
        "filetype": "text",
        "size": 11,
        "mode": "hosted",
        "url_private_download": DOWNLOAD_URL,
    }]);
    frame
}

fn thread_reply(envelope_id: &str, ts: &str, thread_ts: &str, text: &str) -> Value {
    let mut frame = slack_message(envelope_id, SLACK_OWNER, ts, text);
    frame["payload"]["event"]["thread_ts"] = json!(thread_ts);
    frame
}

fn upload_calls(fake: &FakeSlackTransport) -> Vec<SlackFileCall> {
    fake.record()
        .file_calls
        .into_iter()
        .filter(|call| !matches!(call, SlackFileCall::Download { .. }))
        .collect()
}

async fn slack_session(fake: &FakeSlackTransport, n: usize) -> FakeSlackSession {
    bounded(fake.session(n)).await
}

/// A shared file is saved privately, listed in the prompt, answered in the thread, and the
/// file omp sends back goes through the three upload steps into that thread.
#[tokio::test]
async fn e2e_slack_file_share_round_trip() {
    let mut rig = Rig::new(Platform::Slack);
    let fake = Arc::new(FakeSlackTransport::new());
    fake.set_download_bytes(DOWNLOAD_URL, b"hello file\n".to_vec());
    let outbound = rig.outbound_file(b"from omp");
    let inbox = rig.work_dir().join(INBOX_DIR);
    assert!(!inbox.exists(), "no inbox before the first file");
    let running = rig.start(slack_adapter(&fake), "send_file", Some(&outbound));
    let session = slack_session(&fake, 1).await;

    session.frame(file_share("e1", SLACK_OWNER, "1.1", "please read"));
    bounded(fake.wait_until(|record| {
        record
            .file_calls
            .iter()
            .any(|call| matches!(call, SlackFileCall::CompleteUpload { .. }))
    }))
    .await;
    running.stop().await;

    let record = fake.record();
    let download = record
        .file_calls
        .iter()
        .find_map(|call| match call {
            SlackFileCall::Download {
                url_host,
                dest,
                reject_html,
            } => Some((url_host.clone(), dest.clone(), *reject_html)),
            _ => None,
        })
        .expect("one download");
    assert_eq!(download.0, "files.slack.com");
    assert!(download.2, "a text/plain file refuses an HTML login page");

    rig.tap.wait_for_prompts(1).await;
    let prompt = &rig.tap.prompts()[0];
    assert!(
        prompt.starts_with("please read\n\n"),
        "text kept before the block"
    );
    let saved = listed_path(prompt, "1-report.txt");
    assert_eq!(
        saved, download.1,
        "the prompt lists the file the adapter wrote"
    );
    assert!(saved.starts_with(&inbox));
    assert_saved_privately(rig.work_dir(), &saved, b"hello file\n");

    assert_eq!(record.posts.len(), 1, "one reply, no notice");
    let reply = &record.posts[0];
    assert_eq!(reply["channel"], SLACK_CHANNEL);
    assert_eq!(reply["thread_ts"], "1.1");
    assert!(
        reply["text"]
            .as_str()
            .unwrap()
            .starts_with("echo: please read")
    );
    assert_eq!(
        upload_calls(&fake),
        vec![
            SlackFileCall::UploadUrl {
                filename: "result.txt".into(),
                length: 8,
            },
            SlackFileCall::UploadBytes {
                host: "files.slack.com".into(),
                len: 8,
            },
            SlackFileCall::CompleteUpload {
                file_id: FAKE_FILE_ID.into(),
                channel_id: SLACK_CHANNEL.into(),
                thread_ts: Some("1.1".into()),
            },
        ],
        "uploaded after the reply, into the parent thread"
    );
    let row = rig.store().get(1).expect("read row").expect("row exists");
    assert_eq!(row.state, State::Done);
    assert!(row.text_sent);
    rig.tap.assert_no_child_left();
}

/// Slack refuses the second reply's file as too large: no completion, one notice in the
/// thread, the row is done, and a reconnect does not upload it again.
#[tokio::test]
async fn e2e_slack_size_refusal_skips_file_once() {
    let mut rig = Rig::new(Platform::Slack);
    let fake = Arc::new(FakeSlackTransport::new());
    fake.set_download_bytes(DOWNLOAD_URL, b"hello file\n".to_vec());
    let outbound = rig.outbound_file(b"from omp");
    let running = rig.start(slack_adapter(&fake), "send_file", Some(&outbound));
    let session = slack_session(&fake, 1).await;

    session.frame(file_share("e1", SLACK_OWNER, "1.1", "please read"));
    bounded(fake.wait_until(|record| {
        record
            .file_calls
            .iter()
            .any(|call| matches!(call, SlackFileCall::CompleteUpload { .. }))
    }))
    .await;

    fake.fail_next_upload_bytes_status(413);
    session.frame(thread_reply("e2", "2.2", "1.1", "again"));
    // The reply text, then the one file notice.
    bounded(fake.wait_until(|record| record.posts.len() >= 3)).await;
    let notice = escape_text(notice_text(N_FILE_NOT_SENT));
    let record = fake.record();
    assert_eq!(record.posts[1]["thread_ts"], "1.1");
    assert!(
        record.posts[1]["text"]
            .as_str()
            .unwrap()
            .starts_with("echo: again")
    );
    assert_eq!(record.posts[2]["text"], notice.as_str());
    assert_eq!(
        record.posts[2]["thread_ts"], "1.1",
        "the notice goes to the thread"
    );

    // Reconnect, then one more message: the worker handles the reconnect resend before
    // the next row, and the refused file is not among the pending replies.
    session.frame(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    let reconnected = slack_session(&fake, 2).await;
    reconnected.frame(json!({ "type": "hello", "num_connections": 1 }));
    reconnected.frame(thread_reply("e3", "3.3", "1.1", "marker"));
    bounded(fake.wait_until(|record| {
        record
            .file_calls
            .iter()
            .filter(|call| matches!(call, SlackFileCall::CompleteUpload { .. }))
            .count()
            >= 2
    }))
    .await;
    running.stop().await;

    let calls = upload_calls(&fake);
    let count = |pred: fn(&SlackFileCall) -> bool| calls.iter().filter(|c| pred(c)).count();
    assert_eq!(
        count(|c| matches!(c, SlackFileCall::UploadBytes { .. })),
        3,
        "first reply, the refused try, the marker reply: no retry of the refused file"
    );
    assert_eq!(
        count(|c| matches!(c, SlackFileCall::CompleteUpload { .. })),
        2,
        "the refused file was never completed"
    );
    assert!(
        matches!(&calls[4], SlackFileCall::UploadBytes { .. })
            && matches!(&calls[5], SlackFileCall::UploadUrl { .. }),
        "the refused upload stopped before completion"
    );
    let notices = fake
        .record()
        .posts
        .iter()
        .filter(|post| post["text"] == notice.as_str())
        .count();
    assert_eq!(notices, 1, "one notice for the refused file");

    let store = rig.store();
    let refused = store.get(2).expect("read row").expect("row exists");
    assert_eq!(refused.state, State::Done);
    let out: Value = serde_json::from_str(refused.out_files.as_deref().expect("out files"))
        .expect("out files JSON");
    assert_eq!(out[0]["skipped"], true);
    assert_eq!(out[0]["sent"], false);
    assert_eq!(out[0]["attempting"], false);
    assert_eq!(
        store.get(3).expect("read row").expect("row exists").state,
        State::Done
    );
    drop(store);
    rig.tap.wait_for_prompts(3).await;
    rig.tap.assert_no_child_left();
}

/// A `file_share` from someone else is never stored and never downloaded.
#[tokio::test]
async fn e2e_slack_other_user_file_share_is_never_downloaded() {
    let mut rig = Rig::new(Platform::Slack);
    let fake = Arc::new(FakeSlackTransport::new());
    fake.set_download_bytes(DOWNLOAD_URL, b"hello file\n".to_vec());
    let running = rig.start(slack_adapter(&fake), "ok", None);
    let session = slack_session(&fake, 1).await;

    session.frame(file_share("e1", "U999", "1.1", "from a stranger"));
    // Frames are handled in order, so the owner's reply proves the stranger's was seen.
    session.frame(slack_message("e2", SLACK_OWNER, "2.2", "marker"));
    bounded(fake.wait_until(|record| !record.posts.is_empty())).await;
    running.stop().await;

    let store = rig.store();
    assert_eq!(
        store.count().expect("count rows"),
        1,
        "only the owner's row"
    );
    assert_eq!(
        store.get(1).expect("read row").expect("row exists").text,
        "marker"
    );
    drop(store);
    assert!(fake.record().file_calls.is_empty(), "nothing downloaded");
    assert!(!rig.work_dir().join(INBOX_DIR).exists(), "no inbox folder");
    rig.tap.wait_for_prompts(1).await;
    assert_eq!(rig.tap.prompts(), ["marker"]);
    rig.tap.assert_no_child_left();
}

fn discord_message(message_id: &str, text: &str, reference: Option<&str>) -> InboundEvent {
    InboundEvent {
        message_id: message_id.into(),
        author_id: DISCORD_OWNER.into(),
        author_is_bot: false,
        channel_id: DISCORD_CHANNEL.into(),
        thread_id: None,
        is_dm: false,
        text: text.into(),
        attachments: reference
            .map(|reference| {
                vec![InboundAttachment {
                    name: "photo.png".into(),
                    size: 4,
                    mime: Some("image/png".into()),
                    reference: AttachmentRef(reference.into()),
                }]
            })
            .unwrap_or_default(),
    }
}

/// The same file flow with FakeAdapter and Discord-shaped ids: the core is messenger-agnostic.
#[tokio::test]
async fn e2e_fake_adapter_attachment_round_trip() {
    let mut rig = Rig::new(Platform::Discord);
    let adapter = Arc::new(FakeAdapter::new());
    let reference = "fake-attachment-333";
    adapter.set_attachment_bytes(reference, b"\x89PNG".to_vec());
    let outbound = rig.outbound_file(b"from omp");
    let running = rig.start(adapter.clone(), "send_file", Some(&outbound));

    let message_id = "1300000000000000001";
    adapter
        .inject(discord_message(message_id, "", Some(reference)))
        .await;
    bounded(adapter.wait_for_call(|call| matches!(call, Call::SendFile { .. }))).await;
    running.stop().await;

    rig.tap.wait_for_prompts(1).await;
    let prompt = &rig.tap.prompts()[0];
    assert!(
        prompt.starts_with(BLOCK_HEADER),
        "an attachment-only message is the block alone"
    );
    let saved = listed_path(prompt, "1-photo.png");
    assert!(prompt.contains("type: image/png, 4 bytes"));
    assert_saved_privately(rig.work_dir(), &saved, b"\x89PNG");

    let calls: Vec<Call> = adapter
        .calls()
        .into_iter()
        .filter(|call| !matches!(call, Call::StartTyping { .. }))
        .collect();
    let create_thread = calls
        .iter()
        .position(|call| matches!(call, Call::CreateThread { .. }))
        .expect("thread created");
    let fetch = calls
        .iter()
        .position(|call| matches!(call, Call::FetchAttachment { .. }))
        .expect("attachment fetched");
    assert!(
        create_thread < fetch,
        "fetched after the row and its thread"
    );
    assert_eq!(
        calls[fetch],
        Call::FetchAttachment {
            reference: reference.into(),
            dest: saved,
        }
    );
    assert_eq!(
        &calls[fetch + 1..],
        [
            Call::Send {
                channel: message_id.into(),
                text: format!("echo: {prompt}"),
            },
            Call::SendFile {
                target: message_id.into(),
                name: "result.txt".into(),
                bytes: 8,
            },
        ],
        "reply text, then the file, into the thread"
    );
    let row = rig.store().get(1).expect("read row").expect("row exists");
    assert_eq!(row.state, State::Done);
    rig.tap.assert_no_child_left();
}

/// The slice-2 tables, as slice 2 created them (src/store.rs `SCHEMA` at that release).
const SLICE2_SCHEMA: &str = "
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

const NEW_COLUMNS: [&str; 4] = ["attachments", "files_ready", "out_files", "text_sent"];

fn inbox_columns(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("open store file");
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('inbox')")
        .expect("prepare");
    stmt.query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("columns")
}

/// A store file left by slice 2 is migrated in place and keeps serving text unchanged.
#[tokio::test]
async fn e2e_slice2_store_is_migrated_and_serves_text() {
    let mut rig = Rig::new(Platform::Discord);
    let store_file = rig.config.home.join(STORE_FILE);
    {
        let conn = rusqlite::Connection::open(&store_file).expect("create slice-2 store");
        conn.execute_batch(SLICE2_SCHEMA).expect("slice-2 schema");
        conn.execute(
            "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state,
                               reply, notice_sent, received_at, updated_at)
             VALUES ('fake', '1200000000000000001', 'fake:th:1200000000000000001', '111',
                     'old question', 'done', 'old answer', 0, 't', 't')",
            [],
        )
        .expect("slice-2 row");
    }
    for column in NEW_COLUMNS {
        assert!(!inbox_columns(&store_file).iter().any(|c| c == column));
    }

    let adapter = Arc::new(FakeAdapter::new());
    let running = rig.start(adapter.clone(), "ok", None);
    let message_id = "1300000000000000001";
    adapter
        .inject(discord_message(message_id, "hello", None))
        .await;
    bounded(adapter.wait_for_send(1)).await;
    running.stop().await;

    rig.tap.wait_for_prompts(1).await;
    assert_eq!(
        rig.tap.prompts(),
        ["hello"],
        "text-only reaches omp byte for byte"
    );
    let sends: Vec<Call> = adapter
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Send { .. } | Call::SendFile { .. }))
        .collect();
    assert_eq!(
        sends,
        [Call::Send {
            channel: message_id.into(),
            text: "echo: hello".into(),
        }]
    );
    assert!(
        !adapter
            .calls()
            .iter()
            .any(|call| matches!(call, Call::FetchAttachment { .. }))
    );

    let columns = inbox_columns(&store_file);
    for column in NEW_COLUMNS {
        assert!(columns.iter().any(|c| c == column), "missing {column}");
    }
    let store = rig.store();
    let old = store.get(1).expect("read row").expect("old row exists");
    assert_eq!(old.state, State::Done);
    assert_eq!(old.text, "old question");
    assert_eq!(old.reply.as_deref(), Some("old answer"));
    assert!(old.files_ready, "old rows default to files ready");
    assert!(old.attachments.is_none() && old.out_files.is_none());
    let new = store.get(2).expect("read row").expect("new row exists");
    assert_eq!(new.state, State::Done);
    assert!(
        !rig.work_dir().join(INBOX_DIR).exists(),
        "no files, no inbox"
    );
    drop(store);
    rig.tap.assert_no_child_left();
}
