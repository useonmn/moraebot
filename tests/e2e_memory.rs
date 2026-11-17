//! Offline end-to-end tests of owner memory through the library-level gateway:
//! `Gateway::open` makes the memory home, the lead started through the dispatcher gets
//! the memory block, and lanes get only the omp overlay. `FakeAdapter` is the messenger
//! and `fake-omp` is the lead and every lane. Nothing leaves the machine.

use std::fs::OpenOptions;
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::{InboundEvent, MessengerAdapter};
use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
use moraebot::done;
use moraebot::gateway::STORE_FILE;
use moraebot::gateway::{Gateway, GatewayError};
use moraebot::memory::home::MemoryHome;
use moraebot::memory::{
    self, frontmatter, history, home::HUMAN_SEED, home::PERSONA_SEED, provenance,
};
use moraebot::omp;
use moraebot::store::{InboxRow, Store};
use moraebot::testing::FakeAdapter;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const OWNER: &str = "111";
const CHANNEL: &str = "333";
const DM_CHANNEL: &str = "444";
const THREAD: &str = "222";
const BOUND: Duration = Duration::from_secs(30);
const APPEND_FLAG: &str = "--append-system-prompt";

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
        }
    }

    /// Takes in every line written so far: writes an end mark into the FIFO and waits
    /// for it. The FIFO keeps write order, so every earlier line arrives first. Call it
    /// once the children are reaped (the gateway has stopped) or never started.
    async fn drain(&mut self) {
        const MARK: &str = "TAP_END";
        let mut fifo = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .expect("open fifo for writing");
        fifo.write_all(format!("{MARK}\n").as_bytes())
            .expect("write end mark");
        drop(fifo);
        bounded(async {
            loop {
                let line = self.lines.recv().await.expect("log tap open");
                if line == MARK {
                    return;
                }
                self.seen.push(line);
            }
        })
        .await;
    }

    /// `(log index, pid, argv)` of every fake-omp start, as logged.
    fn starts(&self) -> Vec<(usize, u32, Vec<String>)> {
        self.seen
            .iter()
            .enumerate()
            .filter_map(|(at, line)| Some((at, line.strip_prefix("START pid=")?)))
            .map(|(at, rest)| {
                let (pid, argv) = rest.split_once(" argv=").expect("argv");
                let argv: Vec<String> = serde_json::from_str(argv).expect("argv is JSON");
                (at, pid.parse().expect("pid"), argv)
            })
            .collect()
    }

    /// The starts of the lead (`false`) or of lanes (`true`).
    fn starts_of(&self, lane: bool) -> Vec<(usize, u32, Vec<String>)> {
        self.starts()
            .into_iter()
            .filter(|(_, _, argv)| is_lane(argv) == lane)
            .collect()
    }

    /// Tool names of every `set_host_tools` frame logged after log index `from`.
    fn tool_frames_after(&self, from: usize) -> Vec<Vec<String>> {
        self.seen[from..]
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|frame| frame["type"] == "set_host_tools")
            .map(|frame| {
                frame["tools"]
                    .as_array()
                    .expect("tools array")
                    .iter()
                    .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
                    .collect()
            })
            .collect()
    }

    fn assert_no_child_left(&self) {
        for (_, pid, _) in self.starts() {
            assert!(!process_exists(pid), "fake-omp {pid} is still alive");
        }
    }
}

/// A lane child runs with its session dir under the home's `lanes/` folder.
fn is_lane(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg.contains("/lanes/"))
}

/// The `--append-system-prompt` elements of one argv.
fn append_flags(argv: &[String]) -> Vec<&String> {
    argv.iter()
        .filter(|arg| arg.starts_with(APPEND_FLAG))
        .collect()
}

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("run kill")
        .success()
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
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_owned();
        let home = root.join("mbhome");
        let work_dir = root.join("work");
        std::fs::create_dir_all(home.join("secrets")).expect("home");
        std::fs::create_dir_all(&work_dir).expect("work dir");
        let token_file = home.join("secrets").join("discord.token");
        std::fs::write(&token_file, "TOKEN_SENTINEL_9f3a").expect("token file");
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
            .expect("token mode");
        let config = Config {
            messenger: Platform::Discord,
            work_dir,
            omp_path: PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            home,
            access_mode: AccessMode::OwnerOnly,
            discord: Some(DiscordConfig {
                owner_id: OWNER.parse().unwrap(),
                channel_id: CHANNEL.parse().unwrap(),
                token_file,
            }),
            slack: None,
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

    fn memory_root(&self) -> PathBuf {
        self.config.home.join(memory::MEMORY_DIR)
    }

    fn memory_file(&self, rel: &str) -> PathBuf {
        self.memory_root().join(rel)
    }

    /// `Gateway::open` with the lead and lanes getting `FAKE_OMP_LOG` plus `vars`.
    fn open(
        &self,
        adapter: Arc<dyn MessengerAdapter>,
        vars: &[(&str, &str)],
    ) -> Result<Gateway, GatewayError> {
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
        Ok(Gateway::open(&self.config, adapter)?.with_omp(|omp| omp.with_extra_env(env)))
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

/// Copied from `tests/e2e_lead.rs`: test files cannot share private helpers.
fn lane_start_script(task: &str, done: &str) -> String {
    json!({"tool": "lane_start", "arguments": {"task": task, "done": done}}).to_string()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// Runs one DM through a gateway on the rig and returns the lead's single append element.
async fn lead_block_after_one_dm(rig: &mut Rig) -> String {
    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(rig.open(dynamic, &[]).expect("open gateway"));
    adapter
        .inject(dm("1300000000000000001", "hello lead"))
        .await;
    bounded(adapter.wait_for_send(1)).await;
    running.stop().await;
    rig.tap.drain().await;
    rig.tap.assert_no_child_left();

    let leads = rig.tap.starts_of(false);
    assert_eq!(leads.len(), 1, "one lead child");
    let flags = append_flags(&leads[0].2);
    assert_eq!(flags.len(), 1, "one append element");
    flags[0].clone()
}

/// Runs one gateway on the rig with `vars`, sends one DM with `message_id` and returns the
/// append element of the lead start this run added. Each run on a rig needs its own id.
async fn lead_block_after_dm(
    rig: &mut Rig,
    message_id: &str,
    text: &str,
    vars: &[(&str, &str)],
) -> String {
    let before = rig.tap.starts_of(false).len();
    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(rig.open(dynamic, vars).expect("open gateway"));
    adapter.inject(dm(message_id, text)).await;
    bounded(adapter.wait_for_send(1)).await;
    running.stop().await;
    rig.tap.drain().await;
    rig.tap.assert_no_child_left();

    let leads = rig.tap.starts_of(false);
    assert_eq!(leads.len(), before + 1, "one new lead child");
    let flags = append_flags(&leads[before].2);
    assert_eq!(flags.len(), 1, "one append element");
    flags[0].clone()
}

/// The rig's inbox database, opened while no gateway runs.
fn open_store(rig: &Rig) -> Store {
    Store::open(&rig.config.home.join(STORE_FILE)).expect("open store")
}

/// The inbox row of the messenger message `message_id`.
fn inbox_row(store: &Store, message_id: &str) -> InboxRow {
    let count = store.count().expect("count");
    (1..=count)
        .filter_map(|id| store.get(id).expect("get row"))
        .find(|row| row.message_id == message_id)
        .expect("inbox row of the DM")
}

const DIRTY_MARK: &str = "<dirty path=";

#[tokio::test]
async fn first_start_seeds_and_the_lead_sees_the_block() {
    let mut rig = Rig::new();
    assert!(!rig.memory_root().exists(), "fresh home has no memory");

    let block = lead_block_after_one_dm(&mut rig).await;

    let root = rig.memory_root();
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join(memory::SYSTEM_DIR)), 0o700);
    for (rel, seed) in [(memory::PERSONA, PERSONA_SEED), (memory::HUMAN, HUMAN_SEED)] {
        let path = rig.memory_file(rel);
        assert_eq!(mode(&path), 0o600, "{rel}");
        assert_eq!(
            std::fs::read(&path).expect("seed"),
            seed.as_bytes(),
            "{rel}"
        );
    }
    assert!(!rig.memory_file(memory::IDENTITY).exists());

    let block_start = block
        .find(memory::BEGIN)
        .expect("block begins in the append element");
    assert!(block.ends_with(&format!("{}\n", memory::END)));
    let block = &block[block_start..];
    for rel in [memory::PERSONA, memory::HUMAN] {
        assert!(block.contains(&format!("path=\"{rel}\"")), "{rel}");
    }
    assert!(!block.contains(&format!("path=\"{}\"", memory::IDENTITY)));
}

#[tokio::test]
async fn owner_edited_file_is_used_and_never_overwritten() {
    let mut rig = Rig::new();
    let marker = "persona-marker-111";
    let edited = format!("---\ndescription: edited by the owner\n---\n{marker}\n");
    let persona = rig.memory_file(memory::PERSONA);
    std::fs::create_dir_all(persona.parent().unwrap()).expect("system dir");
    std::fs::write(&persona, &edited).expect("write persona");

    let block = lead_block_after_one_dm(&mut rig).await;

    assert_eq!(
        std::fs::read(&persona).expect("persona"),
        edited.as_bytes(),
        "owner file unchanged byte-for-byte"
    );
    assert!(block.contains(marker), "the lead sees the owner's persona");
    let seed_body = frontmatter::parse(PERSONA_SEED).expect("seed parses").body;
    assert!(!block.contains(seed_body), "the seed is not used");
    assert_eq!(
        std::fs::read(rig.memory_file(memory::HUMAN)).expect("human"),
        HUMAN_SEED.as_bytes(),
        "the missing seed is still made"
    );
}

#[tokio::test]
async fn lane_gets_overlay_but_no_memory() {
    let mut rig = Rig::new();
    let (adapter, dynamic) = fake_adapter();
    let script = lane_start_script("summarize the notes", "file .");
    let running = Running::start(
        rig.open(
            dynamic,
            &[
                ("FAKE_OMP_MODE", "tools"),
                ("FAKE_OMP_TOOL_CALLS", &script),
                ("FAKE_OMP_LANE_MODE", "ok"),
            ],
        )
        .expect("open gateway"),
    );
    adapter
        .inject(in_thread(THREAD, "1300000000000000002", "run-tools please"))
        .await;
    // The lead's reply, then its answer to the lane result.
    bounded(adapter.wait_for_send(2)).await;
    running.stop().await;
    rig.tap.drain().await;
    rig.tap.assert_no_child_left();

    let leads = rig.tap.starts_of(false);
    let lanes = rig.tap.starts_of(true);
    assert_eq!(leads.len(), 1, "one lead child");
    assert_eq!(lanes.len(), 1, "one lane child");
    let (lead_at, _, lead_argv) = &leads[0];
    let (lane_at, _, lane_argv) = &lanes[0];
    assert!(lead_at < lane_at, "the lead starts the lane");

    assert_eq!(append_flags(lead_argv).len(), 1, "the lead has the block");
    assert!(append_flags(lead_argv)[0].contains(memory::BEGIN));
    assert!(append_flags(lane_argv).is_empty(), "{lane_argv:?}");

    // The lead registers its tools once, at its start; the next frame is the lane's.
    let lead_tools = &rig.tap.tool_frames_after(*lead_at)[0];
    for tool in [memory::MEMORY_LIST_TOOL, memory::MEMORY_READ_TOOL] {
        assert!(lead_tools.iter().any(|name| name == tool), "{lead_tools:?}");
    }
    let lane_frames = rig.tap.tool_frames_after(*lane_at);
    assert_eq!(lane_frames.len(), 1, "{lane_frames:?}");
    assert_eq!(lane_frames[0], [done::LANE_DONE_TOOL]);

    let overlay = std::path::absolute(omp::overlay_path(&rig.config)).expect("absolute");
    let expected = format!("CONFIG_FILES {}", overlay.display());
    assert!(
        rig.tap.seen[*lane_at..].contains(&expected),
        "the lane logs the overlay path"
    );
    assert_eq!(
        std::fs::read(&overlay).expect("overlay"),
        omp::OMP_OVERLAY.as_bytes()
    );
}

#[tokio::test]
async fn layout_failure_stops_the_gateway_with_code() {
    let mut rig = Rig::new();
    std::fs::write(rig.memory_root(), "not a folder").expect("memory as a file");
    let (_adapter, dynamic) = fake_adapter();

    let error = rig
        .open(dynamic, &[])
        .err()
        .expect("open fails on a broken memory layout");
    assert_eq!(error.code(), memory::E_MEMORY_LAYOUT);

    rig.tap.drain().await;
    assert!(rig.tap.starts().is_empty(), "no omp child started");
    assert!(rig.memory_root().is_file(), "the file is left as it was");
}

#[tokio::test]
async fn lead_write_is_recorded_and_projected_after_restart() {
    const MARKER: &str = "e2e-marker";
    const DM_ID: &str = "1300000000000000011";
    let mut rig = Rig::new();
    let seed_body = frontmatter::parse(PERSONA_SEED).expect("seed parses").body;
    let call = json!({
        "tool": memory::MEMORY_STR_REPLACE_TOOL,
        "arguments": {
            "path": memory::PERSONA,
            "old_string": seed_body,
            "new_string": format!("{MARKER}\n"),
            "reason": "e2e write"
        }
    })
    .to_string();
    let first = lead_block_after_dm(
        &mut rig,
        DM_ID,
        "run-tools please",
        &[("FAKE_OMP_MODE", "tools"), ("FAKE_OMP_TOOL_CALLS", &call)],
    )
    .await;
    assert!(
        !first.contains(MARKER),
        "the first start predates the write"
    );

    let store = open_store(&rig);
    let row = inbox_row(&store, DM_ID);
    let commits = store.memory_commits().expect("commits");
    assert_eq!(commits.len(), 2, "the import and one lead write");
    assert_eq!(commits[0].commit.writer, memory::WRITER_IMPORT);
    let lead = &commits[1];
    assert_eq!(lead.commit.writer, memory::WRITER_LEAD);
    assert_eq!(lead.commit.turn_row, Some(row.id));
    assert_eq!(
        lead.commit.conversation_key.as_deref(),
        Some(row.conversation_key.as_str())
    );
    assert!(lead.applied);
    let snapshots = store.memory_snapshots(lead.id).expect("snapshots");
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(snapshots[0].0, memory::PERSONA);
    let text = snapshots[0].1.clone().expect("persona text");
    assert!(text.contains(MARKER));
    assert_eq!(
        std::fs::read(rig.memory_file(memory::PERSONA)).expect("persona"),
        text.as_bytes(),
        "the working file holds the committed text"
    );
    drop(store);

    let second = lead_block_after_dm(&mut rig, "1300000000000000012", "hello lead", &[]).await;
    assert!(second.contains(MARKER), "the write is projected");
    assert!(!second.contains(DIRTY_MARK), "nothing is dirty");
}

#[tokio::test]
async fn hand_edit_is_held_back_until_accepted() {
    const MARKER: &str = "hand-e2e";
    let mut rig = Rig::new();
    let first = lead_block_after_dm(&mut rig, "1300000000000000021", "hello lead", &[]).await;
    assert!(!first.contains(DIRTY_MARK));
    let store = open_store(&rig);
    assert_eq!(
        store.memory_commits().expect("commits").len(),
        1,
        "the import"
    );
    drop(store);

    std::fs::write(
        rig.memory_file(memory::PERSONA),
        format!("---\ndescription: edited by hand\n---\n{MARKER}\n"),
    )
    .expect("hand edit");

    let held = lead_block_after_dm(&mut rig, "1300000000000000022", "hello lead", &[]).await;
    assert!(!held.contains(MARKER), "the hand edit is not projected");
    assert!(
        held.contains(&format!("{DIRTY_MARK}\"{}\"", memory::PERSONA)),
        "the persona is marked dirty"
    );

    let mut store = open_store(&rig);
    let accepted = history::accept(
        &mut store,
        &MemoryHome::new(&rig.config.home),
        &[],
        history::ACCEPT_REASON,
    )
    .expect("accept");
    assert_eq!(accepted.paths, [memory::PERSONA]);
    let commit = accepted.commit.expect("an accept commit");
    let commits = store.memory_commits().expect("commits");
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[1].id, commit);
    assert_eq!(commits[1].commit.writer, memory::WRITER_HUMAN);
    assert!(commits[1].applied);
    drop(store);

    let accepted = lead_block_after_dm(&mut rig, "1300000000000000023", "hello lead", &[]).await;
    assert!(accepted.contains(MARKER), "the accepted edit is projected");
    assert!(!accepted.contains(DIRTY_MARK), "nothing is dirty");
}

/// The inbox tables of a database made before M2, copied from the slice-4 schema text in
/// `src/store.rs` tests: no memory tables.
const SLICE4_SCHEMA: &str = "
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
        origin TEXT NOT NULL DEFAULT 'owner',
        steered_into INTEGER,
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
    CREATE TABLE sessions(
        name TEXT PRIMARY KEY,
        omp_session_id TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE TABLE lanes(
        id INTEGER PRIMARY KEY,
        tool_call_id TEXT NOT NULL UNIQUE,
        conversation_key TEXT NOT NULL,
        task TEXT NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('running','idle','failed','needs_check')),
        error TEXT,
        prompts INTEGER NOT NULL DEFAULT 1,
        omp_session_id TEXT,
        notice_sent INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
";

#[tokio::test]
async fn old_m1_home_is_imported_on_first_start() {
    const NOTE: &str = "notes/m1.md";
    const NOTE_TEXT: &str = "---\ndescription: a note from M1\n---\nm1-marker\n";
    let mut rig = Rig::new();
    let files = [
        (memory::PERSONA, PERSONA_SEED),
        (memory::HUMAN, HUMAN_SEED),
        (NOTE, NOTE_TEXT),
    ];
    for (rel, text) in files {
        let path = rig.memory_file(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("memory dirs");
        std::fs::write(&path, text).expect("memory file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
    for dir in [
        rig.memory_root(),
        rig.memory_file(memory::SYSTEM_DIR),
        rig.memory_file("notes"),
    ] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
    }
    let conn = rusqlite::Connection::open(rig.config.home.join(STORE_FILE)).expect("old db");
    conn.execute_batch(SLICE4_SCHEMA).expect("old schema");
    drop(conn);

    let block = lead_block_after_one_dm(&mut rig).await;
    assert!(
        !block.contains(DIRTY_MARK),
        "nothing is dirty after the import"
    );

    let store = open_store(&rig);
    let commits = store.memory_commits().expect("commits");
    assert_eq!(commits.len(), 1, "exactly one commit");
    assert_eq!(commits[0].commit.writer, memory::WRITER_IMPORT);
    assert!(commits[0].applied);
    let mut expected: Vec<(String, Option<String>)> = files
        .iter()
        .map(|(rel, text)| ((*rel).to_owned(), Some((*text).to_owned())))
        .collect();
    expected.sort();
    assert_eq!(
        store.memory_snapshots(commits[0].id).expect("snapshots"),
        expected
    );
}

#[tokio::test]
async fn guards_and_index_through_the_gateway() {
    const RO: &str = "notes/ro.md";
    const RO_TEXT: &str = "---\ndescription: locked note\nread_only: \"true\"\n---\nro-marker\n";
    const GONE: &str = "system/gone.md";
    const GONE_TEXT: &str = "---\ndescription: old fact\nexpires: 2000-01-01\n---\ngone-marker\n";
    const DM_ID: &str = "1300000000000000031";
    let mut rig = Rig::new();
    for (rel, text) in [(RO, RO_TEXT), (GONE, GONE_TEXT)] {
        let path = rig.memory_file(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("memory dirs");
        std::fs::write(&path, text).expect("memory file");
    }
    let calls = [
        json!({
            "tool": memory::MEMORY_CREATE_TOOL,
            "arguments": {
                "path": "notes/kept.md",
                "description": "kept note",
                "body": "kept-marker\n",
                "reason": "e2e create"
            }
        }),
        json!({
            "tool": memory::MEMORY_STR_REPLACE_TOOL,
            "arguments": {
                "path": RO,
                "old_string": "ro-marker",
                "new_string": "changed",
                "reason": "e2e locked edit"
            }
        }),
    ]
    .map(|call| call.to_string())
    .join("\n");

    let before = rig.tap.seen.len();
    let first = lead_block_after_dm(
        &mut rig,
        DM_ID,
        "run-tools please",
        &[("FAKE_OMP_MODE", "tools"), ("FAKE_OMP_TOOL_CALLS", &calls)],
    )
    .await;
    assert!(
        !first.contains("gone-marker"),
        "the expired file is left out"
    );
    let results: Vec<(bool, String)> = rig.tap.seen[before..]
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|frame| frame["type"] == "host_tool_result")
        .map(|frame| {
            let text = frame["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            (frame["isError"] == true, text)
        })
        .collect();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    assert!(results[1].0, "{results:?}");
    assert!(
        results[1]
            .1
            .starts_with(&format!("{}:", memory::E_MEMORY_READ_ONLY)),
        "{results:?}"
    );

    let store = open_store(&rig);
    let row = inbox_row(&store, DM_ID);
    let commits = store.memory_commits().expect("commits");
    assert_eq!(commits.len(), 2, "the import and one lead write");
    assert_eq!(commits[0].commit.writer, memory::WRITER_IMPORT);
    assert_eq!(commits[1].commit.writer, memory::WRITER_LEAD);
    assert_eq!(commits[1].commit.turn_row, Some(row.id));
    let snapshots = store.memory_snapshots(commits[1].id).expect("snapshots");
    let paths: Vec<&str> = snapshots.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(paths, ["notes/kept.md"]);
    assert_eq!(
        std::fs::read(rig.memory_file(RO)).expect("locked note"),
        RO_TEXT.as_bytes(),
        "the locked note is unchanged"
    );
    drop(store);

    let second = lead_block_after_dm(&mut rig, "1300000000000000032", "hello lead", &[]).await;
    let index_of = |block: &str| -> String {
        const CLOSE: &str = "</external_projection>\n";
        let at = block
            .find("<external_projection>\n")
            .expect("the notes index is in the block");
        let end = block[at..].find(CLOSE).expect("the notes index is closed") + at;
        block[at..end + CLOSE.len()].to_owned()
    };
    let index = index_of(&second);
    // QA evidence (shown with --nocapture): the notes index of each run, names only.
    println!("gateway 1 index:\n{}", index_of(&first));
    println!("gateway 2 index:\n{index}");
    assert!(
        index.lines().any(|line| line == "notes/: kept, ro"),
        "{index}"
    );
    assert!(
        !second.contains("gone-marker"),
        "the expired file stays out"
    );
    assert!(!second.contains(DIRTY_MARK), "nothing is dirty");
}

#[tokio::test]
async fn owner_approval_through_the_gateway() {
    const PROPOSAL_DM: &str = "1300000000000000021";
    const APPROVAL_DM: &str = "1300000000000000022";
    const APPROVAL: &str = "응";
    const TEA: &str = "system/tea.md";
    let mut rig = Rig::new();

    // Gateway 1: the owner's message and the lead's reply, which becomes the proposal.
    lead_block_after_dm(
        &mut rig,
        PROPOSAL_DM,
        "please remember that I like green tea",
        &[],
    )
    .await;
    let store = open_store(&rig);
    let proposal = inbox_row(&store, PROPOSAL_DM);
    assert_eq!(proposal.id, 1, "the proposal turn is row 1");
    let proposal_reply = proposal.reply.clone().expect("the lead's reply is stored");
    drop(store);

    // Gateway 2: the owner approves; the lead claims the approval once rightly and once
    // with the earlier message, which is outside the turn.
    let calls = [
        json!({
            "tool": memory::MEMORY_CREATE_TOOL,
            "arguments": {
                "path": TEA,
                "description": "tea",
                "body": "green tea\n",
                "reason": "e2e approved",
                "source": "user",
                "user_quote": APPROVAL,
                "quote_message": 2
            }
        }),
        json!({
            "tool": memory::MEMORY_CREATE_TOOL,
            "arguments": {
                "path": "notes/guess.md",
                "description": "guess",
                "body": "likes tea\n",
                "reason": "e2e old quote",
                "source": "user",
                "user_quote": "green tea",
                "quote_message": 1
            }
        }),
    ]
    .map(|call| call.to_string())
    .join("\n");
    lead_block_after_dm(
        &mut rig,
        APPROVAL_DM,
        APPROVAL,
        &[
            ("FAKE_OMP_MODE", "tools"),
            ("FAKE_OMP_TOOL_TRIGGER", APPROVAL),
            ("FAKE_OMP_TOOL_CALLS", &calls),
        ],
    )
    .await;

    let store = open_store(&rig);
    let approval = inbox_row(&store, APPROVAL_DM);
    assert_eq!(approval.id, 2, "the approval turn is row 2");
    let commits = store.memory_commits().expect("commits");
    assert_eq!(commits.len(), 3, "the import and two lead writes");
    assert_eq!(commits[0].commit.writer, memory::WRITER_IMPORT);
    let claims: Vec<Value> = commits[1..]
        .iter()
        .map(|row| {
            assert_eq!(row.commit.writer, memory::WRITER_LEAD);
            assert_eq!(row.commit.turn_row, Some(approval.id));
            let json = row.commit.claim_json.as_deref().expect("claim recorded");
            serde_json::from_str(json).expect("claim JSON")
        })
        .collect();
    let (user, agent) = (&claims[0], &claims[1]);
    assert_eq!(user["source"], "user", "{user}");
    assert_eq!(user["downgraded"], false, "{user}");
    assert_eq!(user["message"], 2, "{user}");
    assert_eq!(user["proposal_row"], 1, "{user}");
    assert_eq!(
        user["proposal_hash"],
        provenance::proposal_hash(&proposal_reply),
        "{user}"
    );
    assert_eq!(user["platform"], approval.platform.as_str(), "{user}");
    assert_eq!(user["message_id"], APPROVAL_DM, "{user}");
    assert_eq!(agent["source"], "agent", "{agent}");
    assert_eq!(agent["requested"], "user", "{agent}");
    assert_eq!(agent["downgraded"], true, "{agent}");
    assert_eq!(agent["code"], memory::E_MEMORY_QUOTE_TURN, "{agent}");
    // QA evidence (shown with --nocapture): the recorded kinds and codes only.
    for claim in &claims {
        println!(
            "claim source={} downgraded={} code={}",
            claim["source"], claim["downgraded"], claim["code"]
        );
    }
    drop(store);

    let tea = std::fs::read_to_string(rig.memory_file(TEA)).expect("tea note");
    let parsed = frontmatter::parse(&tea).expect("tea frontmatter");
    let stamp = parsed
        .frontmatter
        .get(memory::PROVENANCE_KEY)
        .expect("provenance written by moraebot");
    assert!(stamp.starts_with("user:"), "{stamp}");
    assert!(
        std::fs::read_to_string(rig.memory_file("notes/guess.md"))
            .expect("the downgraded write is kept")
            .contains("likes tea")
    );

    // Gateway 3: the lead's block shows the stamp on the approved file.
    let block = lead_block_after_dm(&mut rig, "1300000000000000023", "hello lead", &[]).await;
    let element = block
        .lines()
        .find(|line| line.contains(&format!("path=\"{TEA}\"")))
        .expect("the approved file is in the block");
    assert!(
        element.contains(" provenance=\"user: owner message 2 "),
        "{element}"
    );
}
