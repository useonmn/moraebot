use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
use moraebot::omp::{self, OmpManager, Outcome};
use moraebot::store::Store;
use serde_json::Value;

const KEY_A: &str = "discord:th:1300000000000000001";
const KEY_B: &str = "discord:th:1300000000000000002";
const BOUND: Duration = Duration::from_secs(30);

struct Harness {
    dir: tempfile::TempDir,
    config: Config,
    store: Arc<Mutex<Store>>,
}

impl Harness {
    fn new() -> Self {
        Self::with_omp(OmpConfig::default())
    }

    fn with_omp(omp: OmpConfig) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let work_dir = dir.path().join("work");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&work_dir).expect("work dir");
        let config = Config {
            messenger: Platform::Discord,
            work_dir,
            omp_path: PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
            home: home.clone(),
            access_mode: AccessMode::OwnerOnly,
            discord: Some(DiscordConfig { owner_id: 111, channel_id: 222, token_file: home.join("secrets").join("discord.token") }),
            slack: None,
            omp,
        };
        let mut store = Store::open(&home.join("inbox.sqlite")).expect("store");
        for (n, key) in [KEY_A, KEY_B].into_iter().enumerate() {
            store
                .admit("discord", &format!("m{n}"), key, "222", "111", "seed")
                .expect("admit seed row");
        }
        Self { dir, config, store: Arc::new(Mutex::new(store)) }
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("fake-omp.log")
    }

    fn manager(&self, mode: &str) -> OmpManager {
        OmpManager::new(self.config.clone(), Arc::clone(&self.store)).with_extra_env([
            ("FAKE_OMP_MODE", mode.to_owned()),
            ("FAKE_OMP_LOG", self.log().to_string_lossy().into_owned()),
        ])
    }

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn start_pids(&self) -> Vec<u32> {
        self.log_lines()
            .iter()
            .filter_map(|l| l.strip_prefix("START pid="))
            .map(|rest| rest.split_whitespace().next().unwrap().parse().unwrap())
            .collect()
    }

    fn frames(&self, kind: &str) -> Vec<Value> {
        self.log_lines()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|f| f["type"] == kind)
            .collect()
    }

    fn set_session(&self, key: &str, session_id: &str) {
        self.store.lock().unwrap().set_omp_session_id(key, session_id).unwrap();
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future).await.expect("operation finished within bound")
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run kill -0")
        .success()
}

fn expected_argv(config: &Config, key: &str) -> Vec<String> {
    let session = omp::session_dir(config, key).to_string_lossy().into_owned();
    let work = config.work_dir.to_string_lossy().into_owned();
    ["--mode", "rpc", "--no-ui", "--session-dir", &session, "--cwd", &work, "--approval-mode", "yolo"]
        .map(str::to_owned)
        .to_vec()
}

fn start_argv(line: &str) -> Vec<String> {
    let json = line.split_once("argv=").expect("argv in START line").1;
    serde_json::from_str(json).expect("argv json")
}

#[tokio::test]
async fn argv_is_exact_and_env_drops_only_discord_names() {
    let h = Harness::new();
    let manager = h
        .manager("ok")
        .with_extra_env([("MORAEBOT_DISCORD_PROBE", "x"), ("PROVIDER_PROBE_API_KEY", "y")]);
    let outcome = bounded(manager.prompt(KEY_A, "hi")).await;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    let lines = h.log_lines();
    let start = lines.iter().find(|l| l.starts_with("START pid=")).expect("START line");
    assert_eq!(start_argv(start), expected_argv(&h.config, KEY_A));
    let env = lines.iter().find(|l| l.starts_with("ENV ")).expect("ENV line");
    let names: Vec<&str> = env["ENV ".len()..].split_whitespace().collect();
    assert!(names.iter().all(|n| !n.to_ascii_uppercase().contains("DISCORD")), "{names:?}");
    assert!(names.contains(&"PROVIDER_PROBE_API_KEY"));
    for kept in ["PATH", "HOME"] {
        if std::env::var_os(kept).is_some() {
            assert!(names.contains(&kept), "{kept} must be kept");
        }
    }

    let stderr_log = omp::stderr_log_path(&h.config, KEY_A);
    let mode = std::fs::metadata(&stderr_log).expect("stderr log exists").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        h.store.lock().unwrap().omp_session_id(KEY_A).unwrap().as_deref(),
        Some("fake-session-1")
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn max_cost_adds_flag_only_when_set() {
    let h = Harness::with_omp(OmpConfig { turn_timeout_minutes: None, max_cost_usd: Some(2.5) });
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, "hi")).await;
    let start = h.log_lines().into_iter().find(|l| l.starts_with("START pid=")).unwrap();
    let mut expected = expected_argv(&h.config, KEY_A);
    expected.extend(["--max-cost".to_owned(), "2.5".to_owned()]);
    assert_eq!(start_argv(&start), expected);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn flag_present_when_hints_set() {
    let h = Harness::with_omp(OmpConfig { turn_timeout_minutes: None, max_cost_usd: Some(2.5) });
    let manager = h.manager("ok").with_hints("probe hints".into());
    bounded(manager.prompt(KEY_A, "hi")).await;
    let start = h.log_lines().into_iter().find(|l| l.starts_with("START pid=")).unwrap();
    let mut expected = expected_argv(&h.config, KEY_A);
    expected.extend(["--append-system-prompt=probe hints", "--max-cost", "2.5"].map(str::to_owned));
    assert_eq!(start_argv(&start), expected);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn no_flag_when_hints_empty() {
    let h = Harness::new();
    let manager = h.manager("ok").with_hints(String::new());
    bounded(manager.prompt(KEY_A, "hi")).await;
    let start = h.log_lines().into_iter().find(|l| l.starts_with("START pid=")).unwrap();
    assert!(start_argv(&start).iter().all(|a| !a.starts_with("--append-system-prompt")));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn injection_payload_arrives_byte_for_byte_with_hints_set() {
    let h = Harness::new();
    let manager = h.manager("ok").with_hints("probe hints".into());
    let payload = "$(touch /tmp/moraebot-pwned-7f3a); touch /tmp/moraebot-pwned-7f3a-b";
    let outcome = bounded(manager.prompt(KEY_A, payload)).await;
    assert_eq!(outcome, Outcome::Reply(format!("echo: {payload}")));
    let prompts = h.frames("prompt");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["message"], payload);
    assert!(!Path::new("/tmp/moraebot-pwned-7f3a").exists());
    assert!(!Path::new("/tmp/moraebot-pwned-7f3a-b").exists());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn multibyte_text_round_trips() {
    let h = Harness::new();
    let manager = h.manager("ok");
    let text = "안녕하세요, 모래봇 🌊";
    let outcome = bounded(manager.prompt(KEY_A, text)).await;
    assert_eq!(outcome, Outcome::Reply(format!("echo: {text}")));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn injection_payload_arrives_byte_for_byte_and_never_runs() {
    let h = Harness::new();
    let manager = h.manager("ok");
    let payload = "$(touch /tmp/moraebot-pwned-7f3a); touch /tmp/moraebot-pwned-7f3a-b";
    let outcome = bounded(manager.prompt(KEY_A, payload)).await;
    assert_eq!(outcome, Outcome::Reply(format!("echo: {payload}")));
    let prompts = h.frames("prompt");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["message"], payload);
    let start = h.log_lines().into_iter().find(|l| l.starts_with("START pid=")).unwrap();
    assert!(start_argv(&start).iter().all(|a| !a.contains("moraebot-pwned")));
    assert!(!Path::new("/tmp/moraebot-pwned-7f3a").exists());
    assert!(!Path::new("/tmp/moraebot-pwned-7f3a-b").exists());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn not_invoked_is_no_agent() {
    let h = Harness::new();
    let manager = h.manager("not_invoked");
    assert_eq!(bounded(manager.prompt(KEY_A, "hi")).await, Outcome::NoAgent);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn error_status_is_failed() {
    let h = Harness::new();
    let manager = h.manager("error");
    let outcome = bounded(manager.prompt(KEY_A, "hi")).await;
    assert!(matches!(outcome, Outcome::Failed { .. }));
    assert_eq!(outcome.code(), Some(omp::E_PROMPT_ERROR));
    manager.shutdown_all().await;
}

/// Shell stand-in for omp whose error turn carries `error.code: "provider_error"`: it runs
/// fake-omp (mode `error`) and adds the code to the `prompt_result` frame on its way out.
/// Written into the test's temp dir.
fn coded_error_wrapper(root: &Path) -> PathBuf {
    let script = root.join("omp-coded-error.sh");
    let fake = env!("CARGO_BIN_EXE_fake-omp");
    let body = format!(
        r#"#!/bin/sh
'{fake}' "$@" | awk '{{ sub(/"message":"fake provider failure"/, "\"code\":\"provider_error\",\"message\":\"fake provider failure\""); print; fflush() }}'
"#
    );
    std::fs::write(&script, body).expect("write wrapper");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    script
}

#[tokio::test]
async fn coded_non_cost_cap_error_is_prompt_error() {
    let mut h = Harness::new();
    h.config.omp_path = coded_error_wrapper(h.dir.path());
    let manager = h.manager("error");
    let outcome = bounded(manager.prompt(KEY_A, "hi")).await;
    assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
    assert_eq!(outcome.code(), Some(omp::E_PROMPT_ERROR));
    assert!(manager.child_pid().await.is_some(), "a finished turn keeps its child");
    assert_eq!(h.frames("prompt").len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn hang_with_turn_timeout_is_needs_check_and_child_is_reaped() {
    let h = Harness::new();
    let manager = h.manager("hang").with_turn_timeout(Duration::from_secs(1));
    let outcome = bounded(manager.prompt(KEY_A, "hi")).await;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }));
    assert_eq!(outcome.code(), Some(omp::E_TURN_TIMEOUT));
    let pids = h.start_pids();
    assert_eq!(pids.len(), 1);
    assert!(!alive(pids[0]), "timed-out child must be gone");
    assert_eq!(manager.child_pid().await, None);
    assert_eq!(h.frames("prompt").len(), 1);
}

#[tokio::test]
async fn open_session_failure_is_failed_without_prompt() {
    let h = Harness::new();
    let manager = h.manager("open_session_fail");
    let outcome = bounded(manager.prompt(KEY_A, "hi")).await;
    assert!(matches!(outcome, Outcome::Failed { .. }));
    assert_eq!(outcome.code(), Some(omp::E_OPEN_SESSION));
    assert!(h.frames("prompt").is_empty());
    assert_eq!(manager.child_pid().await, None);
    assert!(!alive(h.start_pids()[0]));
}

#[tokio::test]
async fn held_lease_is_needs_check_without_prompt() {
    let h = Harness::new();
    h.set_session(KEY_A, "fake-session-1");
    let manager = h.manager("lease_held");
    let outcome = bounded(manager.prompt(KEY_A, "hi")).await;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }));
    assert_eq!(outcome.code(), Some(omp::E_SESSION_BUSY));
    let leases = h.frames("get_session_lease");
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0]["sessionIds"], serde_json::json!(["fake-session-1"]));
    assert!(h.frames("open_session").is_empty());
    assert!(h.frames("prompt").is_empty());
    assert!(!alive(h.start_pids()[0]));
}

#[tokio::test]
async fn reopen_checks_free_lease_then_prompts() {
    let h = Harness::new();
    h.set_session(KEY_A, "fake-session-1");
    let manager = h.manager("ok");
    assert_eq!(bounded(manager.prompt(KEY_A, "hi")).await, Outcome::Reply("echo: hi".into()));
    assert_eq!(h.frames("get_session_lease").len(), 1);
    assert_eq!(h.frames("prompt").len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn fresh_conversation_skips_lease_check() {
    let h = Harness::new();
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, "hi")).await;
    assert!(h.frames("get_session_lease").is_empty());
    assert_eq!(h.frames("open_session").len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn one_child_in_total_across_conversations() {
    let h = Harness::new();
    let manager = h.manager("ok");

    bounded(manager.prompt(KEY_A, "one")).await;
    bounded(manager.prompt(KEY_A, "two")).await;
    assert_eq!(h.start_pids().len(), 1, "same conversation reuses its child");

    bounded(manager.prompt(KEY_B, "three")).await;
    let pids = h.start_pids();
    assert_eq!(pids.len(), 2);
    assert!(!alive(pids[0]), "previous child is reaped before the next spawns");

    bounded(manager.prompt(KEY_A, "four")).await;
    let pids = h.start_pids();
    assert_eq!(pids.len(), 3);
    assert!(!alive(pids[1]));
    assert_eq!(manager.child_pid().await, Some(pids[2]));

    let sessions: Vec<String> = h
        .log_lines()
        .iter()
        .filter(|l| l.starts_with("START pid="))
        .map(|l| start_argv(l)[4].clone())
        .collect();
    assert_eq!(sessions[0], sessions[2]);
    assert_ne!(sessions[0], sessions[1]);

    manager.shutdown_all().await;
    assert!(!alive(pids[2]));
    assert_eq!(manager.child_pid().await, None);
}

#[tokio::test]
async fn killed_child_is_lost_then_respawned() {
    let h = Harness::new();
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, "first")).await;
    let pid = manager.child_pid().await.expect("live child");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success());

    let outcome = bounded(manager.prompt(KEY_A, "second")).await;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }));
    assert_eq!(outcome.code(), Some(omp::E_CHILD_LOST));
    assert_eq!(manager.child_pid().await, None);
    assert!(!alive(pid));

    let outcome = bounded(manager.prompt(KEY_A, "third")).await;
    assert_eq!(outcome, Outcome::Reply("echo: third".into()));
    assert_eq!(h.start_pids().len(), 2);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn exit1_is_lost() {
    let h = Harness::new();
    let manager = h.manager("exit1");
    let outcome = bounded(manager.prompt(KEY_A, "hi")).await;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }), "{outcome:?}");
    assert_eq!(outcome.code(), Some(omp::E_CHILD_LOST));
    assert_eq!(manager.child_pid().await, None);
    let pids = h.start_pids();
    assert_eq!(pids.len(), 1);
    assert!(!alive(pids[0]), "exited child must be gone");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn missing_omp_binary_is_spawn_failure() {
    let mut h = Harness::new();
    h.config.omp_path = h.dir.path().join("no-such-omp");
    let manager = h.manager("ok");
    let outcome = bounded(manager.prompt(KEY_A, "hi")).await;
    assert_eq!(outcome.code(), Some(omp::E_OMP_SPAWN));
}
