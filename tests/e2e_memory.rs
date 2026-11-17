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
use moraebot::gateway::{Gateway, GatewayError};
use moraebot::memory::{self, frontmatter, home::HUMAN_SEED, home::PERSONA_SEED};
use moraebot::omp;
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
        rig.tap.seen[*lane_at..]
            .iter()
            .any(|line| *line == expected),
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
