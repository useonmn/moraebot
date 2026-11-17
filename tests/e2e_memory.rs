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
use moraebot::dispatch::{N_ONBOARDING_HELLO, notice_text};
use moraebot::done;
use moraebot::gateway::STORE_FILE;
use moraebot::gateway::{Gateway, GatewayError};
use moraebot::memory::home::MemoryHome;
use moraebot::memory::{
    self, frontmatter, history, home::HUMAN_SEED, home::PERSONA_SEED, notice, onboarding,
    provenance,
};
use moraebot::omp;
use moraebot::store::{InboxRow, OnboardingRow, OnboardingState, Store};
use moraebot::testing::{Call, FakeAdapter};
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

#[tokio::test]
async fn memory_repin_and_notices_through_the_gateway() {
    const MARKER: &str = "tool-marker";
    let mut rig = Rig::new();
    let seed_body = frontmatter::parse(PERSONA_SEED).expect("seed parses").body;
    let call = json!({
        "tool": memory::MEMORY_STR_REPLACE_TOOL,
        "arguments": {
            "path": memory::PERSONA,
            "old_string": seed_body,
            "new_string": format!("{MARKER}\n"),
            "reason": "r",
            "source": "agent"
        }
    })
    .to_string();
    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(
        rig.open(
            dynamic,
            &[
                ("FAKE_OMP_MODE", "tools"),
                ("FAKE_OMP_TOOL_CALLS", &call),
                ("FAKE_OMP_COMPACT_TRIGGER", "compact-now"),
            ],
        )
        .expect("open gateway"),
    );
    // Turn 1: the lead writes its persona and the conversation is compacted.
    adapter
        .inject(dm("1300000000000000031", "run-tools compact-now"))
        .await;
    bounded(adapter.wait_for_send(1)).await;
    // Turn 2: the compaction changed the block, so the lead is re-pinned by a restart.
    adapter.inject(dm("1300000000000000032", "hi")).await;
    bounded(adapter.wait_for_send(2)).await;
    // Turn 3: the owner's refresh request restarts the lead with an unchanged block.
    let refresh = notice::refresh_file(&rig.config.home);
    std::fs::write(&refresh, "").expect("refresh request");
    adapter.inject(dm("1300000000000000033", "again")).await;
    bounded(adapter.wait_for_send(3)).await;
    running.stop().await;
    rig.tap.drain().await;
    rig.tap.assert_no_child_left();

    let leads = rig.tap.starts_of(false);
    // QA evidence (shown with --nocapture): the lead start pids only.
    for (_, pid, _) in &leads {
        println!("lead START pid={pid}");
    }
    assert_eq!(
        leads.len(),
        3,
        "first start, compaction re-pin, refresh re-pin"
    );
    let session_dirs: std::collections::BTreeSet<&String> = leads
        .iter()
        .map(|(_, _, argv)| {
            let at = argv
                .iter()
                .position(|arg| arg == "--session-dir")
                .expect("--session-dir flag");
            &argv[at + 1]
        })
        .collect();
    assert_eq!(session_dirs.len(), 1, "every restart keeps the session");
    let blocks: Vec<&String> = leads
        .iter()
        .map(|(_, _, argv)| {
            let flags = append_flags(argv);
            assert_eq!(flags.len(), 1, "one append element");
            flags[0]
        })
        .collect();
    assert!(
        !blocks[0].contains(MARKER),
        "the first pin predates the write"
    );
    assert!(
        blocks[1].contains(MARKER),
        "the compaction re-pin has the write"
    );
    assert!(
        blocks[2].contains(MARKER),
        "the refresh re-pin has the write"
    );

    let prompts: Vec<String> = rig
        .tap
        .seen
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|frame| frame["type"] == "prompt")
        .map(|frame| {
            frame["message"]
                .as_str()
                .expect("prompt message")
                .to_owned()
        })
        .collect();
    assert_eq!(prompts.len(), 3, "three lead prompts");
    assert!(
        !prompts[0].contains(memory::NOTICE_BEGIN),
        "nothing to say at the first prompt"
    );
    let second: Vec<&str> = prompts[1].lines().collect();
    assert_eq!(second[1], memory::NOTICE_BEGIN, "{second:?}");
    assert!(second.contains(&"compaction count=1"), "{second:?}");
    assert!(second.contains(&memory::NOTICE_END), "{second:?}");
    assert!(
        !second.iter().any(|line| line.starts_with("changed kind=")),
        "the lead's own write is not announced: {second:?}"
    );
    assert!(
        !prompts[2].contains(memory::NOTICE_BEGIN),
        "the compaction is told once"
    );
    assert!(!refresh.exists(), "the restart removed the refresh request");
}

#[tokio::test]
async fn agreements_and_boundaries_through_the_gateway() {
    const RULE: &str = "never push to main";
    let mut rig = Rig::new();
    let calls = [
        json!({
            "tool": memory::MEMORY_AGREEMENT_RECORD_TOOL,
            "arguments": {"scope": "global", "text": RULE, "reason": "r"}
        }),
        json!({
            "tool": memory::MEMORY_AGREEMENT_BREAK_TOOL,
            "arguments": {"id": 1, "reason": "r"}
        }),
        json!({
            "tool": memory::MEMORY_CREATE_TOOL,
            "arguments": {
                "path": memory::BOUNDARIES,
                "description": "b",
                "body": format!("- {RULE}\n"),
                "reason": "r",
                "source": "agent"
            }
        }),
    ]
    .map(|call| call.to_string())
    .join("\n");
    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(
        rig.open(
            dynamic,
            &[("FAKE_OMP_MODE", "tools"), ("FAKE_OMP_TOOL_CALLS", &calls)],
        )
        .expect("open gateway"),
    );
    // Turns 1-3: each owner message runs the three calls; the break counts once per turn.
    for (n, id) in [
        "1300000000000000041",
        "1300000000000000042",
        "1300000000000000043",
    ]
    .into_iter()
    .enumerate()
    {
        adapter.inject(dm(id, "run-tools")).await;
        bounded(adapter.wait_for_send(n + 1)).await;
    }
    // Turn 4: the newly flagged agreement is told to the lead once.
    adapter.inject(dm("1300000000000000044", "hi")).await;
    bounded(adapter.wait_for_send(4)).await;
    // Turn 5: the owner's refresh re-pins the lead with the ledger in its block.
    let refresh = notice::refresh_file(&rig.config.home);
    std::fs::write(&refresh, "").expect("refresh request");
    adapter.inject(dm("1300000000000000045", "again")).await;
    bounded(adapter.wait_for_send(5)).await;
    running.stop().await;
    rig.tap.drain().await;
    rig.tap.assert_no_child_left();

    let leads = rig.tap.starts_of(false);
    // QA evidence (shown with --nocapture): the lead start pids and counts only.
    for (_, pid, _) in &leads {
        println!("lead START pid={pid}");
    }
    assert_eq!(leads.len(), 2, "first start, refresh re-pin");
    let blocks: Vec<&String> = leads
        .iter()
        .map(|(_, _, argv)| {
            let flags = append_flags(argv);
            assert_eq!(flags.len(), 1, "one append element");
            flags[0]
        })
        .collect();
    assert!(
        !blocks[0].contains("<agreements>\n"),
        "the first pin predates the ledger"
    );
    assert!(blocks[1].contains("<agreements>\n"), "the re-pin lists it");
    let item = format!("- #1 [global] [agent] [broken x3] NEEDS-HUMAN: {RULE}");
    assert!(
        blocks[1].lines().any(|line| line == item),
        "the re-pin shows the flagged item"
    );

    let frames: Vec<Value> = rig
        .tap
        .seen
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let prompts: Vec<&str> = frames
        .iter()
        .filter(|frame| frame["type"] == "prompt")
        .map(|frame| frame["message"].as_str().expect("prompt message"))
        .collect();
    println!("prompts={}", prompts.len());
    assert_eq!(prompts.len(), 5, "five lead prompts");
    let fourth: Vec<&str> = prompts[3].lines().collect();
    assert_eq!(fourth[1], memory::NOTICE_BEGIN, "{fourth:?}");
    let line = format!("agreement needs-human id=1 broken=3 text=\"{RULE}\"");
    assert!(fourth.contains(&line.as_str()), "{fourth:?}");
    for (at, prompt) in prompts.iter().enumerate() {
        let count = prompt
            .lines()
            .filter(|line| line.starts_with("agreement "))
            .count();
        assert_eq!(count, usize::from(at == 3), "prompt {at}: {prompt:?}");
    }

    let results: Vec<(bool, &str)> = frames
        .iter()
        .filter(|frame| frame["type"] == "host_tool_result")
        .map(|frame| {
            let text = frame["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default();
            (frame["isError"] == true, text)
        })
        .collect();
    println!("host_tool_results={}", results.len());
    assert_eq!(results.len(), 9, "{results:?}");
    for at in [2, 5, 8] {
        assert!(results[at].0, "{results:?}");
        assert!(
            results[at]
                .1
                .starts_with(&format!("{}:", memory::E_MEMORY_QUOTE)),
            "{results:?}"
        );
    }
    assert!(!rig.memory_file(memory::BOUNDARIES).exists());

    let agreements = open_store(&rig).agreements().expect("agreements");
    assert_eq!(agreements.len(), 1, "{agreements:?}");
    assert_eq!(agreements[0].broken, 3);
    assert!(agreements[0].needs_human);
}

/// The lines of one prompt's notice block that start with `onboarding `.
fn onboarding_lines(prompt: &str) -> Vec<&str> {
    prompt
        .lines()
        .skip_while(|line| *line != memory::NOTICE_BEGIN)
        .take_while(|line| *line != memory::NOTICE_END)
        .filter(|line| line.starts_with("onboarding "))
        .collect()
}

/// The `Send` calls one fake adapter recorded, as `(channel, text)`.
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

#[tokio::test]
async fn onboarding_through_the_gateway() {
    let mut rig = Rig::new();
    onboarding::request(&rig.config.home).expect("onboarding request");
    let request = onboarding::request_file(&rig.config.home);
    let calls = [
        json!({
            "tool": memory::MEMORY_CREATE_TOOL,
            "arguments": {
                "path": memory::IDENTITY,
                "description": "What the lead is called",
                "body": "Name: Sora\n",
                "reason": "r",
                "source": "user",
                "user_quote": "Call yourself Sora",
                "quote_message": 2
            }
        }),
        json!({
            "tool": memory::ONBOARDING_FINISH_TOOL,
            "arguments": {"outcome": "done", "reason": "r"}
        }),
    ]
    .map(|call| call.to_string())
    .join("\n");
    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(
        rig.open(
            dynamic,
            &[("FAKE_OMP_MODE", "tools"), ("FAKE_OMP_TOOL_CALLS", &calls)],
        )
        .expect("open gateway"),
    );
    // Startup: the request turns into one greeting in the configured channel.
    bounded(adapter.wait_for_send(1)).await;
    assert_eq!(
        adapter.calls().first(),
        Some(&Call::Send {
            channel: CHANNEL.to_owned(),
            text: notice_text(N_ONBOARDING_HELLO).to_owned(),
        })
    );
    assert!(!request.exists(), "the greeting consumed the request");
    // Row 1: the owner's first reply.
    adapter.inject(dm("1300000000000000051", "hi")).await;
    bounded(adapter.wait_for_send(2)).await;
    // Row 2: the lead saves the name with the owner's words and ends the meeting.
    adapter
        .inject(dm("1300000000000000052", "Call yourself Sora. run-tools"))
        .await;
    bounded(adapter.wait_for_send(3)).await;
    // Row 3: the refresh request re-pins the lead before this prompt.
    adapter.inject(dm("1300000000000000053", "again")).await;
    bounded(adapter.wait_for_send(4)).await;
    running.stop().await;
    let first_sends = sends(&adapter);

    // A second gateway on the same home: no greeting, no onboarding line.
    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(
        rig.open(dynamic, &[("FAKE_OMP_MODE", "ok")])
            .expect("open gateway"),
    );
    adapter.inject(dm("1300000000000000054", "hi")).await;
    bounded(adapter.wait_for_send(1)).await;
    running.stop().await;
    let second_sends = sends(&adapter);
    rig.tap.drain().await;
    rig.tap.assert_no_child_left();

    let to_channel = first_sends
        .iter()
        .filter(|(channel, _)| channel == CHANNEL)
        .count();
    assert_eq!(to_channel, 1, "{first_sends:?}");
    assert_eq!(second_sends.len(), 1, "{second_sends:?}");
    assert_eq!(second_sends[0].0, DM_CHANNEL);

    let leads = rig.tap.starts_of(false);
    // QA evidence (shown with --nocapture): the lead start pids and counts only.
    for (_, pid, _) in &leads {
        println!("lead START pid={pid}");
    }
    assert_eq!(
        leads.len(),
        3,
        "first start, refresh re-pin, second gateway"
    );
    let flags = append_flags(&leads[1].2);
    assert_eq!(flags.len(), 1, "one append element");
    assert!(flags[0].contains("Name: Sora"), "the re-pin has the name");

    let prompts: Vec<String> = rig
        .tap
        .seen
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|frame| frame["type"] == "prompt")
        .map(|frame| {
            frame["message"]
                .as_str()
                .expect("prompt message")
                .to_owned()
        })
        .collect();
    println!("prompts={}", prompts.len());
    assert_eq!(prompts.len(), 4, "four lead prompts");
    assert_eq!(onboarding_lines(&prompts[0]), ["onboarding turn=1"]);
    assert_eq!(onboarding_lines(&prompts[1]), ["onboarding turn=2"]);
    for (at, prompt) in prompts.iter().enumerate().skip(2) {
        assert!(
            !prompt.lines().any(|line| line.starts_with("onboarding ")),
            "prompt {at}: {prompt:?}"
        );
    }

    let identity = std::fs::read_to_string(rig.memory_file(memory::IDENTITY)).expect("identity");
    let parsed = frontmatter::parse(&identity).expect("identity frontmatter");
    let stamp = parsed
        .frontmatter
        .get(memory::PROVENANCE_KEY)
        .expect("provenance written by moraebot");
    assert!(
        stamp.starts_with("quoted: owner message 2 contains \"Call yourself Sora\""),
        "{stamp}"
    );
    assert_eq!(
        open_store(&rig).onboarding().expect("onboarding row"),
        Some(OnboardingRow {
            state: OnboardingState::Done,
            after_row: 0
        })
    );
}

/// The lines of one prompt's notice block that start with `pressure `.
fn pressure_lines(prompt: &str) -> Vec<&str> {
    prompt
        .lines()
        .skip_while(|line| *line != memory::NOTICE_BEGIN)
        .take_while(|line| *line != memory::NOTICE_END)
        .filter(|line| line.starts_with("pressure "))
        .collect()
}

/// The value of attribute `name` in one `<pressure ...>` line.
fn pressure_attr<'a>(line: &'a str, name: &str) -> &'a str {
    let start = format!(" {name}=\"");
    let at = line.find(&start).expect("attribute present") + start.len();
    let len = line[at..].find('"').expect("attribute closes");
    &line[at..at + len]
}

#[tokio::test]
async fn memory_pressure_through_the_gateway() {
    const PRESSURE_LINE: &str = "<pressure tokens=\"";
    let mut rig = Rig::new();
    // 105000 bytes of system/ files before the first start: 28 bytes of frontmatter, 14971
    // `y`s and a newline each.
    let filler = format!("---\ndescription: filler\n---\n{}\n", "y".repeat(14971));
    assert_eq!(filler.len(), 15000);
    let system = rig.memory_root().join(memory::SYSTEM_DIR);
    std::fs::create_dir_all(&system).expect("system dir");
    for n in 1..=7 {
        std::fs::write(system.join(format!("p{n}.md")), &filler).expect("write filler");
    }
    let call = json!({
        "tool": memory::MEMORY_CREATE_TOOL,
        "arguments": {
            "path": "system/p8.md",
            "description": "filler",
            "body": format!("{}\n", "y".repeat(15200)),
            "reason": "r",
            "source": "agent"
        }
    })
    .to_string();
    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(
        rig.open(
            dynamic,
            &[("FAKE_OMP_MODE", "tools"), ("FAKE_OMP_TOOL_CALLS", &call)],
        )
        .expect("open gateway"),
    );
    // Turn 1: under the budget; the lead writes one more file and crosses it.
    adapter.inject(dm("1300000000000000051", "run-tools")).await;
    bounded(adapter.wait_for_send(1)).await;
    // Turn 2: the crossing is told once.
    adapter.inject(dm("1300000000000000052", "hi")).await;
    bounded(adapter.wait_for_send(2)).await;
    // Turn 3: still over the budget, already told.
    adapter.inject(dm("1300000000000000053", "again")).await;
    bounded(adapter.wait_for_send(3)).await;
    running.stop().await;
    rig.tap.drain().await;
    rig.tap.assert_no_child_left();

    let leads = rig.tap.starts_of(false);
    // QA evidence (shown with --nocapture): the lead start pids and counts only.
    for (_, pid, _) in &leads {
        println!("lead START pid={pid}");
    }
    assert_eq!(leads.len(), 1, "a lead write does not restart the lead");
    let flags = append_flags(&leads[0].2);
    assert_eq!(flags.len(), 1, "one append element");
    let lines: Vec<&str> = flags[0].lines().collect();
    let at: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with(PRESSURE_LINE))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(at.len(), 1, "one pressure line in the block");
    let line = lines[at[0]];
    let tokens: usize = pressure_attr(line, "tokens").parse().expect("tokens");
    println!("block pressure tokens={tokens}");
    assert_eq!(pressure_attr(line, "budget"), "30000");
    assert!((24000..30000).contains(&tokens), "tokens={tokens}");
    assert_eq!(pressure_attr(line, "code"), memory::E_MEMORY_PRESSURE);
    assert_eq!(lines[at[0] + 1], "</memory_metadata>");

    let frames: Vec<Value> = rig
        .tap
        .seen
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let results: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["type"] == "host_tool_result")
        .collect();
    println!("host_tool_results={}", results.len());
    assert_eq!(results.len(), 1);
    assert_ne!(results[0]["isError"], true, "{}", results[0]);

    let prompts: Vec<&str> = frames
        .iter()
        .filter(|frame| frame["type"] == "prompt")
        .map(|frame| frame["message"].as_str().expect("prompt message"))
        .collect();
    println!("prompts={}", prompts.len());
    assert_eq!(prompts.len(), 3, "three lead prompts");
    let told = pressure_lines(prompts[1]);
    assert_eq!(told.len(), 1, "{:?}", prompts[1]);
    let fields: Vec<&str> = told[0].split(' ').collect();
    assert_eq!(fields.len(), 5, "{fields:?}");
    assert_eq!(fields[..2], ["pressure", "over_budget"]);
    let t2: usize = fields[2]
        .strip_prefix("tokens=")
        .expect("tokens field")
        .parse()
        .expect("tokens");
    println!("notice pressure tokens={t2}");
    assert_eq!(fields[3], "budget=30000");
    let percent: usize = fields[4]
        .strip_prefix("percent=")
        .expect("percent field")
        .parse()
        .expect("percent");
    assert!(t2 >= 30000, "tokens={t2}");
    assert_eq!(percent, t2 * 100 / 30000);
    for at in [0, 2] {
        assert!(
            !prompts[at]
                .lines()
                .any(|line| line.starts_with("pressure ")),
            "prompt {at}: {:?}",
            prompts[at]
        );
    }
}

#[tokio::test]
async fn people_and_patches_through_the_gateway() {
    const CARD: &str = "---\ndescription: About Kim\nkind: person\n---\nIDENTITY: Kim\nATTRIBUTE: works on the build\n";
    let mut rig = Rig::new();
    let add_two = "*** Begin Patch\n\
                   *** Add File: people/kim/card.md\n\
                   +---\n\
                   +description: About Kim\n\
                   +kind: person\n\
                   +---\n\
                   +IDENTITY: Kim\n\
                   +ATTRIBUTE: works on the build\n\
                   *** Add File: notes/kim.md\n\
                   +---\n\
                   +description: notes on Kim\n\
                   +---\n\
                   +met in the build channel\n\
                   *** End Patch";
    let move_and_edit = "*** Begin Patch\n\
                         *** Update File: notes/kim.md\n\
                         *** Move to: notes/kim-notes.md\n\
                         @@\n\
                         -met in the build channel\n\
                         +met in the release channel\n\
                         *** End Patch";
    let one_bad_hunk = "*** Begin Patch\n\
                        *** Add File: notes/later.md\n\
                        +---\n\
                        +description: later\n\
                        +---\n\
                        +a later note\n\
                        *** Update File: notes/kim-notes.md\n\
                        @@\n\
                        -not in the file\n\
                        *** End Patch";
    let patch = |text: &str| {
        json!({
            "tool": memory::MEMORY_APPLY_PATCH_TOOL,
            "arguments": {"patch": text, "reason": "r"}
        })
    };
    let observe = |slug: &str, text: &str| {
        json!({
            "tool": memory::MEMORY_OBSERVE_TOOL,
            "arguments": {"slug": slug, "text": text, "reason": "r"}
        })
    };
    let calls = [
        patch(add_two),
        patch(move_and_edit),
        patch(one_bad_hunk),
        observe("kim", "likes tea"),
        observe("kim", "Likes tea"),
        observe("human", "prefers short replies"),
    ]
    .map(|call| call.to_string())
    .join("\n");
    let (adapter, dynamic) = fake_adapter();
    let running = Running::start(
        rig.open(
            dynamic,
            &[("FAKE_OMP_MODE", "tools"), ("FAKE_OMP_TOOL_CALLS", &calls)],
        )
        .expect("open gateway"),
    );
    adapter.inject(dm("1300000000000000061", "run-tools")).await;
    bounded(adapter.wait_for_send(1)).await;
    running.stop().await;
    rig.tap.drain().await;
    rig.tap.assert_no_child_left();

    let leads = rig.tap.starts_of(false);
    // QA evidence (shown with --nocapture): the lead start pids and counts only.
    for (_, pid, _) in &leads {
        println!("lead START pid={pid}");
    }
    assert_eq!(leads.len(), 1, "a lead write does not restart the lead");

    let frames = rig.tap.tool_frames_after(0);
    let names = frames.first().expect("the lead frame");
    let at = names
        .iter()
        .position(|name| name == memory::MEMORY_APPLY_PATCH_TOOL)
        .expect("apply_patch registered");
    assert_eq!(names[at - 1], memory::ONBOARDING_FINISH_TOOL, "{names:?}");
    assert_eq!(names[at + 1], memory::MEMORY_OBSERVE_TOOL, "{names:?}");
    assert!(
        names[at + 2..]
            .iter()
            .all(|name| !name.starts_with("memory_")),
        "{names:?}"
    );

    let frames: Vec<Value> = rig
        .tap
        .seen
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|frame| frame["type"] == "host_tool_result")
        .collect();
    println!("host_tool_results={}", frames.len());
    assert_eq!(frames.len(), 6, "{frames:?}");
    let errors: Vec<bool> = frames
        .iter()
        .map(|frame| frame["isError"] == true)
        .collect();
    assert_eq!(
        errors,
        [false, false, true, false, false, false],
        "{frames:?}"
    );
    let texts: Vec<&str> = frames
        .iter()
        .map(|frame| {
            frame["result"]["content"][0]["text"]
                .as_str()
                .expect("result text")
        })
        .collect();
    assert!(
        texts[2].starts_with(&format!("{}:", memory::E_MEMORY_PATCH)),
        "{texts:?}"
    );
    let json_of = |at: usize| -> Value { serde_json::from_str(texts[at]).expect("result JSON") };
    assert_eq!(
        json_of(0)["paths"],
        json!(["notes/kim.md", "people/kim/card.md"])
    );
    assert_eq!(
        json_of(1)["paths"],
        json!(["notes/kim-notes.md", "notes/kim.md"])
    );
    for (at, n) in [(3, 1), (4, 2), (5, 1)] {
        assert_eq!(json_of(at)["n"], json!(n), "result {}", at + 1);
    }

    let read = |rel: &str| std::fs::read_to_string(rig.memory_file(rel)).expect("memory file");
    assert_eq!(
        std::fs::read(rig.memory_file("people/kim/card.md")).expect("card"),
        CARD.as_bytes()
    );
    assert!(!rig.memory_file("notes/kim.md").exists());
    assert!(!rig.memory_file("notes/later.md").exists());
    assert!(read("notes/kim-notes.md").ends_with("met in the release channel\n"));
    let observations = read("people/kim/observations.md");
    let entries: Vec<&str> = observations
        .lines()
        .filter(|line| line.starts_with("- ["))
        .collect();
    assert_eq!(entries.len(), 1, "{observations}");
    assert!(
        entries[0].ends_with("] likes tea <!-- n=2 -->"),
        "{observations}"
    );
    let human = read(memory::HUMAN);
    let headings = human
        .lines()
        .filter(|line| *line == memory::EXPLICIT_SECTION)
        .count();
    assert_eq!(headings, 1, "{human}");
    let entries: Vec<&str> = human
        .lines()
        .filter(|line| line.starts_with("- ["))
        .collect();
    assert_eq!(entries.len(), 1, "{human}");
    assert!(
        entries[0].ends_with("] prefers short replies <!-- n=1 -->"),
        "{human}"
    );

    let commits = open_store(&rig).memory_commits().expect("commits");
    let lead_commits = commits
        .iter()
        .filter(|row| row.commit.writer == memory::WRITER_LEAD)
        .count();
    println!("lead commits={lead_commits}");
    assert_eq!(lead_commits, 5, "patches 1 and 2 and three observes");
}
