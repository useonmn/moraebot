//! The lead's memory extension through `OmpManager` and fake-omp: the extension is written
//! and offered with `-e` at each lead start with memory, the ack decides whether a re-pin
//! happens in place or by restart, and notices go to the outbox instead of the owner's text.

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
use moraebot::lead;
use moraebot::memory::extension::{self, BLOCK_FILE, EXT_FILE, OUTBOX_DIR};
use moraebot::memory::home::MemoryHome;
use moraebot::memory::{self, history, notice};
use moraebot::omp::OmpManager;
use moraebot::store::{Admitted, Store};
use serde_json::Value;

const KEY: &str = "discord:th:111";
const COMPACT: (&str, &str) = ("FAKE_OMP_COMPACT_TRIGGER", "compact-now");
const STALE: &str = "moraebot-memory-0";
const BOUND: Duration = Duration::from_secs(30);

struct Harness {
    dir: tempfile::TempDir,
    config: Config,
    store: Arc<Mutex<Store>>,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("qa-mbhome");
        let work_dir = dir.path().join("work");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&work_dir).expect("work dir");
        let config = Config {
            messenger: Platform::Discord,
            work_dir,
            omp_path: PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            home: home.clone(),
            access_mode: AccessMode::OwnerOnly,
            discord: Some(DiscordConfig {
                owner_id: 111,
                channel_id: 222,
                token_file: home.join("secrets").join("discord.token"),
            }),
            slack: None,
            omp: OmpConfig::default(),
        };
        let store = Store::open(&home.join("inbox.sqlite")).expect("store");
        Self {
            dir,
            config,
            store: Arc::new(Mutex::new(store)),
        }
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("fake-omp.log")
    }

    /// The fake's base variables, plus the ack version when `ack` is set.
    fn env(&self, ack: Option<&str>, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut vars = vec![
            ("FAKE_OMP_MODE".to_owned(), "ok".to_owned()),
            (
                "FAKE_OMP_LOG".to_owned(),
                self.log().to_string_lossy().into_owned(),
            ),
        ];
        if let Some(ack) = ack {
            vars.push(("FAKE_OMP_EXT_ACK".to_owned(), ack.to_owned()));
        }
        vars.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        vars
    }

    /// A lead manager with memory whose fake acks `ack` (none: no ack).
    fn manager(&self, ack: Option<&str>, extra: &[(&str, &str)]) -> OmpManager {
        OmpManager::new(self.config.clone(), Arc::clone(&self.store))
            .with_extra_env(self.env(ack, extra))
            .with_memory(self.memory())
    }

    fn memory(&self) -> MemoryHome {
        let home = MemoryHome::new(&self.config.home);
        home.ensure_layout().expect("memory layout");
        home
    }

    /// Commits a hand edit of the persona whose body is `marker`.
    fn change_persona(&self, marker: &str) {
        let path = self
            .config
            .home
            .join(memory::MEMORY_DIR)
            .join(memory::PERSONA);
        std::fs::write(path, format!("---\ndescription: d\n---\n{marker}\n"))
            .expect("write persona");
        history::accept(&mut self.store.lock().unwrap(), &self.memory(), &[], "test")
            .expect("accept the hand edit");
    }

    /// Admits and claims an owner row of `KEY`; returns its id.
    fn owner_row(&self, message_id: &str) -> i64 {
        let mut store = self.store.lock().unwrap();
        let id = match store
            .admit("discord", message_id, KEY, "222", "111", "body")
            .expect("admit")
        {
            Admitted::New(id) => id,
            Admitted::Duplicate => panic!("duplicate message id"),
        };
        assert!(store.claim(id).expect("claim"));
        id
    }

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Log index and argv of every `START` line.
    fn starts(&self) -> Vec<(usize, Vec<String>)> {
        self.log_lines()
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("START pid="))
            .map(|(at, l)| {
                let json = l.split_once("argv=").expect("argv in START line").1;
                (at, serde_json::from_str(json).expect("argv json"))
            })
            .collect()
    }

    /// The environment names of the `ENV` line right after the `START` line at `start`.
    fn env_names(&self, start: usize) -> Vec<String> {
        let lines = self.log_lines();
        let line = lines
            .get(start + 1)
            .and_then(|l| l.strip_prefix("ENV "))
            .expect("ENV line after START");
        line.split(' ').map(str::to_owned).collect()
    }

    /// Log index and `message` of every `prompt` frame.
    fn prompts(&self) -> Vec<(usize, String)> {
        self.log_lines()
            .iter()
            .enumerate()
            .filter_map(|(at, l)| {
                let frame = serde_json::from_str::<Value>(l).ok()?;
                (frame["type"] == "prompt")
                    .then(|| (at, frame["message"].as_str().expect("message").to_owned()))
            })
            .collect()
    }

    /// Log index and decoded value of every `OUTBOX` line.
    fn outbox(&self) -> Vec<(usize, String)> {
        self.log_lines()
            .iter()
            .enumerate()
            .filter_map(|(at, l)| {
                let json = l.strip_prefix("OUTBOX ")?;
                Some((at, serde_json::from_str(json).expect("outbox json")))
            })
            .collect()
    }

    fn ext_dir(&self) -> PathBuf {
        extension::dir(&self.config.home)
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("operation finished within bound")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// The `--append-system-prompt=` value of one argv.
fn append_of(argv: &[String]) -> &str {
    argv.iter()
        .find_map(|a| a.strip_prefix("--append-system-prompt="))
        .expect("append flag")
}

/// Sends a tagged owner input for a fresh owner row.
async fn owner_prompt(h: &Harness, manager: &OmpManager, message_id: &str, body: &str) {
    let row = h.owner_row(message_id);
    bounded(manager.prompt(KEY, row, &lead::owner_input(KEY, row, body))).await;
}

/// Prompt 1 compacts the conversation, then the persona changes and prompt 2 runs.
async fn compact_then_change(h: &Harness, manager: &OmpManager) {
    owner_prompt(h, manager, "m-1", "compact-now").await;
    h.change_persona("v2-marker-111");
    owner_prompt(h, manager, "m-2", "hi").await;
}

#[tokio::test]
async fn lead_start_installs_and_loads_the_extension() {
    let h = Harness::new();
    let manager = h.manager(Some(extension::VERSION), &[]);
    let row = h.owner_row("m-0");
    bounded(manager.prompt(KEY, row, "hi")).await;

    let starts = h.starts();
    assert_eq!(starts.len(), 1);
    let (at, argv) = &starts[0];
    let ext = std::path::absolute(h.ext_dir().join(EXT_FILE)).expect("absolute");
    let flags: Vec<usize> = (0..argv.len()).filter(|i| argv[*i] == "-e").collect();
    assert_eq!(flags.len(), 1);
    assert_eq!(
        argv.get(flags[0] + 1).map(String::as_str),
        Some(ext.to_str().expect("utf-8 path"))
    );
    assert_eq!(
        std::fs::read(&ext).expect("extension file"),
        extension::SOURCE.as_bytes()
    );
    assert_eq!(mode(&ext), 0o600);
    assert!(h.env_names(*at).iter().any(|n| n == extension::HOME_ENV));
    assert_eq!(
        std::fs::read_to_string(h.ext_dir().join(BLOCK_FILE)).expect("block file"),
        extension::block_of(append_of(argv)).expect("memory block")
    );
    let ack_line = format!("EXT_ACK {}", extension::VERSION);
    assert!(h.log_lines().contains(&ack_line));
    assert!(manager.extension_loaded());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn without_memory_there_is_no_extension() {
    let h = Harness::new();
    let manager = OmpManager::new(h.config.clone(), Arc::clone(&h.store))
        .with_extra_env(h.env(Some(extension::VERSION), &[]));
    bounded(manager.prompt(KEY, 1, "hi")).await;

    let starts = h.starts();
    assert_eq!(starts.len(), 1, "reach: the lead started");
    let (at, argv) = &starts[0];
    assert!(!argv.iter().any(|a| a == "-e"));
    assert!(!h.env_names(*at).iter().any(|n| n == extension::HOME_ENV));
    assert!(!h.ext_dir().join(EXT_FILE).exists());
    assert!(!h.log_lines().iter().any(|l| l.starts_with("EXT_ACK")));
    assert!(!manager.extension_loaded());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn missing_or_stale_ack_keeps_the_restart_refresh() {
    for ack in [None, Some(STALE)] {
        let h = Harness::new();
        let manager = h.manager(ack, &[COMPACT]);
        compact_then_change(&h, &manager).await;

        let starts = h.starts();
        assert!(
            starts[0].1.iter().any(|a| a == "-e"),
            "reach: the extension was offered ({ack:?})"
        );
        if let Some(stale) = ack {
            let line = format!("EXT_ACK {stale}");
            assert!(h.log_lines().contains(&line), "reach: a stale ack existed");
        }
        assert!(!manager.extension_loaded(), "{ack:?}");
        assert_eq!(starts.len(), 2, "re-pinned by restart ({ack:?})");
        assert!(append_of(&starts[1].1).contains("v2-marker-111"));

        let prompts = h.prompts();
        let lines: Vec<&str> = prompts[1].1.split('\n').collect();
        assert!(lines.contains(&memory::NOTICE_BEGIN), "{ack:?}");
        assert!(lines.contains(&"compaction count=1"), "{ack:?}");
        assert!(h.outbox().is_empty(), "{ack:?}");
        manager.shutdown_all().await;
    }
}

#[tokio::test]
async fn compaction_and_refresh_repin_in_place() {
    let h = Harness::new();
    let manager = h.manager(Some(extension::VERSION), &[COMPACT]);
    compact_then_change(&h, &manager).await;

    let starts = h.starts();
    assert_eq!(starts.len(), 1, "re-pinned without a restart");
    assert!(
        !append_of(&starts[0].1).contains("v2-marker-111"),
        "reach: the argv still holds the old block"
    );
    let block_file = h.ext_dir().join(BLOCK_FILE);
    let block = std::fs::read_to_string(&block_file).expect("block file");
    assert!(block.contains("v2-marker-111"));
    assert!(block.starts_with(memory::BEGIN));
    assert!(block.ends_with(&format!("{}\n", memory::END)));

    let prompts = h.prompts();
    assert!(
        prompts
            .iter()
            .all(|(_, m)| !m.contains(memory::NOTICE_BEGIN))
    );
    let outbox = h.outbox();
    assert_eq!(outbox.len(), 1);
    assert!(outbox[0].0 > prompts[1].0);
    let lines: Vec<&str> = outbox[0].1.lines().collect();
    assert!(lines.contains(&memory::NOTICE_BEGIN));
    assert!(lines.contains(&"compaction count=1"));
    assert!(lines.contains(&memory::NOTICE_END));
    assert!(!lines.iter().any(|l| l.starts_with("soul ")));

    let refresh = notice::refresh_file(&h.config.home);
    std::fs::write(&refresh, "").expect("refresh request");
    owner_prompt(&h, &manager, "m-3", "again").await;
    assert_eq!(h.starts().len(), 1);
    assert!(!refresh.exists());
    assert_eq!(h.outbox().len(), 1);

    h.change_persona("v3-marker-111");
    owner_prompt(&h, &manager, "m-4", "later").await;
    assert_eq!(h.starts().len(), 1);
    let block = std::fs::read_to_string(&block_file).expect("block file");
    assert!(block.contains("v2-marker-111"));
    assert!(
        !block.contains("v3-marker-111"),
        "a write alone keeps the pin"
    );
    let outbox = h.outbox();
    assert_eq!(outbox.len(), 2);
    let soul = format!("soul path={}", serde_json::Value::from(memory::PERSONA));
    assert!(outbox[1].1.lines().any(|l| l.starts_with(&soul)));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn outbox_failure_falls_back_to_the_envelope() {
    let h = Harness::new();
    let manager = h.manager(Some(extension::VERSION), &[]);
    owner_prompt(&h, &manager, "m-1", "hello").await;
    assert!(manager.extension_loaded(), "reach: the extension is loaded");

    let outbox_dir = h.ext_dir().join(OUTBOX_DIR);
    std::fs::remove_dir_all(&outbox_dir).expect("remove outbox");
    std::fs::write(&outbox_dir, "").expect("outbox as a file");
    h.change_persona("v2-marker-111");
    owner_prompt(&h, &manager, "m-2", "hi").await;

    let prompts = h.prompts();
    let lines: Vec<&str> = prompts[1].1.split('\n').collect();
    assert!(lines.contains(&memory::NOTICE_BEGIN));
    assert!(lines.iter().any(|l| l.starts_with("soul path=")));
    assert!(h.outbox().is_empty());
    assert_eq!(h.starts().len(), 1);
    manager.shutdown_all().await;
}
