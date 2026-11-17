use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
use moraebot::done;
use moraebot::files;
use moraebot::lane;
use moraebot::lead;
use moraebot::memory::home::{MemoryHome, PERSONA_SEED};
use moraebot::memory::{self, frontmatter, history, notice};
use moraebot::omp::{self, GroupReport, OmpManager, Outcome};
use moraebot::store::{
    AgreementAdded, LaneStart, LaneState, NewAgreement, OnboardingRow, OnboardingState, Store,
};
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
            discord: Some(DiscordConfig {
                owner_id: 111,
                channel_id: 222,
                token_file: home.join("secrets").join("discord.token"),
            }),
            slack: None,
            omp,
        };
        let mut store = Store::open(&home.join("inbox.sqlite")).expect("store");
        for (n, key) in [KEY_A, KEY_B].into_iter().enumerate() {
            store
                .admit("discord", &format!("m{n}"), key, "222", "111", "seed")
                .expect("admit seed row");
        }
        Self {
            dir,
            config,
            store: Arc::new(Mutex::new(store)),
        }
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

    /// Values of the `CONFIG_FILES` lines the fake logged, in order.
    fn config_files(&self) -> Vec<String> {
        self.log_lines()
            .iter()
            .filter_map(|l| l.strip_prefix("CONFIG_FILES "))
            .map(str::to_owned)
            .collect()
    }

    fn set_session(&self, session_id: &str) {
        self.store
            .lock()
            .unwrap()
            .set_session_id(lead::LEAD_SESSION, session_id)
            .unwrap();
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("operation finished within bound")
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run kill -0")
        .success()
}

fn expected_argv(config: &Config) -> Vec<String> {
    let session = omp::lead_session_dir(config).to_string_lossy().into_owned();
    let work = config.work_dir.to_string_lossy().into_owned();
    [
        "--mode",
        "rpc",
        "--no-ui",
        "--session-dir",
        &session,
        "--cwd",
        &work,
        "--approval-mode",
        "yolo",
    ]
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
    let manager = h.manager("ok").with_extra_env([
        ("MORAEBOT_DISCORD_PROBE", "x"),
        ("PROVIDER_PROBE_API_KEY", "y"),
    ]);
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    let lines = h.log_lines();
    let start = lines
        .iter()
        .find(|l| l.starts_with("START pid="))
        .expect("START line");
    assert_eq!(start_argv(start), expected_argv(&h.config));
    let env = lines
        .iter()
        .find(|l| l.starts_with("ENV "))
        .expect("ENV line");
    let names: Vec<&str> = env["ENV ".len()..].split_whitespace().collect();
    assert!(
        names
            .iter()
            .all(|n| !n.to_ascii_uppercase().contains("DISCORD")),
        "{names:?}"
    );
    assert!(names.contains(&"PROVIDER_PROBE_API_KEY"));
    for kept in ["PATH", "HOME"] {
        if std::env::var_os(kept).is_some() {
            assert!(names.contains(&kept), "{kept} must be kept");
        }
    }

    let stderr_log = omp::stderr_log_path(&h.config, lead::LEAD_SESSION);
    let mode = std::fs::metadata(&stderr_log)
        .expect("stderr log exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        h.store
            .lock()
            .unwrap()
            .session_id(lead::LEAD_SESSION)
            .unwrap()
            .as_deref(),
        Some("fake-session-1")
    );
    manager.shutdown_all().await;
}

fn assert_overlay_file(path: &Path) {
    assert_eq!(
        std::fs::read(path).expect("overlay"),
        omp::OMP_OVERLAY.as_bytes()
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
}

#[tokio::test]
async fn lead_gets_the_overlay_env_and_file() {
    let h = Harness::new();
    let manager = h.manager("ok");
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    let overlay = omp::overlay_path(&h.config);
    assert_eq!(h.config_files(), [overlay.to_string_lossy()]);
    assert_overlay_file(&overlay);
    let lines = h.log_lines();
    let start = lines
        .iter()
        .find(|l| l.starts_with("START pid="))
        .expect("START line");
    assert_eq!(start_argv(start), expected_argv(&h.config));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn inherited_config_files_value_is_replaced() {
    let h = Harness::new();
    let overlay = omp::overlay_path(&h.config);
    let folder = overlay.parent().unwrap();
    std::fs::create_dir_all(folder).unwrap();
    std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&overlay, b"memory:\n  backend: local\n").unwrap();
    let manager = h
        .manager("ok")
        .with_extra_env([(omp::CONFIG_FILES_ENV, "/elsewhere.yml")]);
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    assert_eq!(h.config_files(), [overlay.to_string_lossy()]);
    assert_overlay_file(&overlay);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn overlay_path_is_absolute_for_a_relative_home() {
    std::fs::create_dir_all("target").unwrap();
    let tmp = tempfile::Builder::new()
        .prefix("qa-mbhome")
        .tempdir_in("target")
        .unwrap();
    let cwd = std::env::current_dir().unwrap();
    let rel = tmp.path().strip_prefix(&cwd).unwrap().to_path_buf();
    assert!(rel.is_relative());
    let mut h = Harness::new();
    h.config.home = rel;
    let manager = h.manager("ok");
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    let values = h.config_files();
    assert_eq!(values.len(), 1, "{values:?}");
    let value = Path::new(&values[0]);
    assert!(value.is_absolute(), "{value:?}");
    assert_eq!(value, cwd.join(omp::overlay_path(&h.config)));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn max_cost_adds_flag_only_when_set() {
    let h = Harness::with_omp(OmpConfig {
        turn_timeout_minutes: None,
        max_cost_usd: Some(2.5),
    });
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, 1, "hi")).await;
    let start = h
        .log_lines()
        .into_iter()
        .find(|l| l.starts_with("START pid="))
        .unwrap();
    let mut expected = expected_argv(&h.config);
    expected.extend(["--max-cost".to_owned(), "2.5".to_owned()]);
    assert_eq!(start_argv(&start), expected);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn flag_present_when_hints_set() {
    let h = Harness::with_omp(OmpConfig {
        turn_timeout_minutes: None,
        max_cost_usd: Some(2.5),
    });
    let manager = h.manager("ok").with_hints("probe hints".into());
    bounded(manager.prompt(KEY_A, 1, "hi")).await;
    let start = h
        .log_lines()
        .into_iter()
        .find(|l| l.starts_with("START pid="))
        .unwrap();
    let mut expected = expected_argv(&h.config);
    expected.extend(["--append-system-prompt=probe hints", "--max-cost", "2.5"].map(str::to_owned));
    assert_eq!(start_argv(&start), expected);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn no_flag_when_hints_empty() {
    let h = Harness::new();
    let manager = h.manager("ok").with_hints(String::new());
    bounded(manager.prompt(KEY_A, 1, "hi")).await;
    let start = h
        .log_lines()
        .into_iter()
        .find(|l| l.starts_with("START pid="))
        .unwrap();
    assert!(
        start_argv(&start)
            .iter()
            .all(|a| !a.starts_with("--append-system-prompt"))
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn injection_payload_arrives_byte_for_byte_with_hints_set() {
    let h = Harness::new();
    let manager = h.manager("ok").with_hints("probe hints".into());
    let payload = "$(touch /tmp/moraebot-pwned-7f3a); touch /tmp/moraebot-pwned-7f3a-b";
    let outcome = bounded(manager.prompt(KEY_A, 1, payload))
        .await
        .main
        .outcome;
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
    let outcome = bounded(manager.prompt(KEY_A, 1, text)).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply(format!("echo: {text}")));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn injection_payload_arrives_byte_for_byte_and_never_runs() {
    let h = Harness::new();
    let manager = h.manager("ok");
    let payload = "$(touch /tmp/moraebot-pwned-7f3a); touch /tmp/moraebot-pwned-7f3a-b";
    let outcome = bounded(manager.prompt(KEY_A, 1, payload))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply(format!("echo: {payload}")));
    let prompts = h.frames("prompt");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["message"], payload);
    let start = h
        .log_lines()
        .into_iter()
        .find(|l| l.starts_with("START pid="))
        .unwrap();
    assert!(
        start_argv(&start)
            .iter()
            .all(|a| !a.contains("moraebot-pwned"))
    );
    assert!(!Path::new("/tmp/moraebot-pwned-7f3a").exists());
    assert!(!Path::new("/tmp/moraebot-pwned-7f3a-b").exists());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn not_invoked_is_no_agent() {
    let h = Harness::new();
    let manager = h.manager("not_invoked");
    assert_eq!(
        bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome,
        Outcome::NoAgent
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn error_status_is_failed() {
    let h = Harness::new();
    let manager = h.manager("error");
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
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
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
    assert_eq!(outcome.code(), Some(omp::E_PROMPT_ERROR));
    assert!(
        manager.child_pid().await.is_some(),
        "a finished turn keeps its child"
    );
    assert_eq!(h.frames("prompt").len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn hang_with_turn_timeout_is_needs_check_and_child_is_reaped() {
    let h = Harness::new();
    let manager = h.manager("hang").with_turn_timeout(Duration::from_secs(1));
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
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
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert!(matches!(outcome, Outcome::Failed { .. }));
    assert_eq!(outcome.code(), Some(omp::E_OPEN_SESSION));
    assert!(h.frames("prompt").is_empty());
    assert_eq!(manager.child_pid().await, None);
    assert!(!alive(h.start_pids()[0]));
}

#[tokio::test]
async fn held_lease_is_needs_check_without_prompt() {
    let h = Harness::new();
    h.set_session("fake-session-1");
    let manager = h.manager("lease_held");
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }));
    assert_eq!(outcome.code(), Some(omp::E_SESSION_BUSY));
    let leases = h.frames("get_session_lease");
    assert_eq!(leases.len(), 1);
    assert_eq!(
        leases[0]["sessionIds"],
        serde_json::json!(["fake-session-1"])
    );
    assert!(h.frames("open_session").is_empty());
    assert!(h.frames("prompt").is_empty());
    assert!(!alive(h.start_pids()[0]));
}

#[tokio::test]
async fn reopen_checks_free_lease_then_prompts() {
    let h = Harness::new();
    h.set_session("fake-session-1");
    let manager = h.manager("ok");
    assert_eq!(
        bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome,
        Outcome::Reply("echo: hi".into())
    );
    assert_eq!(h.frames("get_session_lease").len(), 1);
    assert_eq!(h.frames("prompt").len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn fresh_conversation_skips_lease_check() {
    let h = Harness::new();
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, 1, "hi")).await;
    assert!(h.frames("get_session_lease").is_empty());
    assert_eq!(h.frames("open_session").len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn one_lead_child_serves_every_conversation() {
    let h = Harness::new();
    let manager = h.manager("ok");

    let first = bounded(manager.prompt("discord:th:111", 1, "one")).await;
    assert_eq!(first.main.outcome, Outcome::Reply("echo: one".into()));
    let pid = manager.child_pid().await.expect("live lead child");
    let second = bounded(manager.prompt("discord:dm:222", 2, "two")).await;
    assert_eq!(second.main.outcome, Outcome::Reply("echo: two".into()));
    assert_eq!(
        manager.child_pid().await,
        Some(pid),
        "the lead child is kept"
    );

    let starts: Vec<String> = h
        .log_lines()
        .into_iter()
        .filter(|l| l.starts_with("START pid="))
        .collect();
    assert_eq!(starts.len(), 1, "one lead child for every conversation");
    assert!(start_argv(&starts[0])[4].ends_with("lead/session"));

    manager.shutdown_all().await;
    assert!(!alive(pid));
    assert_eq!(manager.child_pid().await, None);
}

#[tokio::test]
async fn lead_session_id_is_stored_and_lease_checked() {
    let h = Harness::new();
    let first = h.manager("ok");
    bounded(first.prompt(KEY_A, 1, "hi")).await;
    assert_eq!(
        h.store
            .lock()
            .unwrap()
            .session_id(lead::LEAD_SESSION)
            .unwrap()
            .as_deref(),
        Some("fake-session-1")
    );
    first.shutdown_all().await;

    let second = h.manager("ok");
    assert_eq!(
        bounded(second.prompt(KEY_B, 2, "again")).await.main.outcome,
        Outcome::Reply("echo: again".into())
    );
    second.shutdown_all().await;
    let lines = h.log_lines();
    let second_start = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("START pid="))
        .nth(1)
        .expect("second START line")
        .0;
    let opening: Vec<Value> = lines[second_start..]
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|f| f["type"] == "get_session_lease" || f["type"] == "open_session")
        .collect();
    assert_eq!(opening[0]["type"], "get_session_lease");
    assert_eq!(
        opening[0]["sessionIds"],
        serde_json::json!(["fake-session-1"])
    );
    assert_eq!(opening[1]["type"], "open_session");

    let third = h.manager("lease_held");
    let outcome = bounded(third.prompt(KEY_A, 3, "busy")).await.main.outcome;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }), "{outcome:?}");
    assert_eq!(outcome.code(), Some(omp::E_SESSION_BUSY));
}

#[tokio::test]
async fn killed_child_is_lost_then_respawned() {
    let h = Harness::new();
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, 1, "first")).await;
    let pid = manager.child_pid().await.expect("live child");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success());

    let outcome = bounded(manager.prompt(KEY_A, 2, "second"))
        .await
        .main
        .outcome;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }));
    assert_eq!(outcome.code(), Some(omp::E_CHILD_LOST));
    assert_eq!(manager.child_pid().await, None);
    assert!(!alive(pid));

    let outcome = bounded(manager.prompt(KEY_A, 3, "third"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: third".into()));
    assert_eq!(h.start_pids().len(), 2);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn exit1_is_lost() {
    let h = Harness::new();
    let manager = h.manager("exit1");
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
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
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome.code(), Some(omp::E_OMP_SPAWN));
}

// --- send_file host tool -------------------------------------------------------------

impl Harness {
    /// A `send_file` manager whose fake omp calls the tool once per line of `lines`.
    fn send_file_manager(&self, lines: &[String]) -> OmpManager {
        self.manager("send_file")
            .with_extra_env([("FAKE_OMP_SEND_FILE", lines.join("\n"))])
    }

    /// Creates `rel` (and its parents) under the work dir and returns its canonical path.
    fn work_file(&self, rel: &str) -> PathBuf {
        let path = self.config.work_dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("parent dir");
        std::fs::write(&path, b"report").expect("write work file");
        path.canonicalize().expect("canonical work file")
    }

    /// `(isError, text)` of every `host_tool_result` frame, in order.
    fn tool_results(&self) -> Vec<(bool, String)> {
        self.frames("host_tool_result")
            .iter()
            .map(|f| {
                let text = f["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                (f["isError"] == true, text)
            })
            .collect()
    }
}

#[tokio::test]
async fn send_file_tool_records_inside_path() {
    let h = Harness::new();
    let file = h.work_file("s3-report.txt");
    let manager = h.send_file_manager(&[file.to_string_lossy().into_owned()]);
    let report = bounded(manager.prompt(KEY_A, 1, "hi")).await;
    assert_eq!(report.main.outcome, Outcome::Reply("echo: hi".into()));
    assert_eq!(report.main.files, [file]);
    let results = h.tool_results();
    assert_eq!(results.len(), 1);
    assert!(!results[0].0, "{results:?}");
    let tools = h.frames("set_host_tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["tools"][0]["name"], omp::SEND_FILE_TOOL);
    assert_eq!(tools[0]["tools"][0]["loadMode"], "essential");
    // QA evidence (shown with --nocapture): the host tool frames fake omp received.
    for frame in tools.iter().chain(h.frames("host_tool_result").iter()) {
        println!("FRAME {frame}");
    }
    manager.shutdown_all().await;
}

#[tokio::test]
async fn send_file_tool_accepts_relative_path() {
    let h = Harness::new();
    assert!(
        !Path::new("out/s3-rel-report.txt").exists(),
        "the test's own current folder must not hold the file"
    );
    let file = h.work_file("out/s3-rel-report.txt");
    std::fs::write(h.dir.path().join("s3-outside.txt"), b"outside").expect("outside file");
    let manager = h.send_file_manager(&[
        "out/s3-rel-report.txt".to_owned(),
        "../s3-outside.txt".to_owned(),
    ]);
    let report = bounded(manager.prompt(KEY_A, 1, "hi")).await;
    assert_eq!(report.main.outcome, Outcome::Reply("echo: hi".into()));
    assert_eq!(report.main.files, [file]);
    let results = h.tool_results();
    assert_eq!(results.len(), 2);
    assert!(!results[0].0, "{results:?}");
    assert!(results[1].0, "{results:?}");
    assert!(results[1].1.starts_with(files::E_FILE_PATH), "{results:?}");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn send_file_tool_rejects_outside_path() {
    let h = Harness::new();
    let other = tempfile::tempdir().expect("other tempdir");
    let outside = other.path().join("s3-elsewhere.txt");
    std::fs::write(&outside, b"elsewhere").expect("outside file");
    let manager = h.send_file_manager(&[outside.to_string_lossy().into_owned()]);
    let report = bounded(manager.prompt(KEY_A, 1, "hi")).await;
    assert_eq!(report.main.outcome, Outcome::Reply("echo: hi".into()));
    assert!(report.main.files.is_empty());
    let results = h.tool_results();
    assert_eq!(results.len(), 1);
    assert!(results[0].0, "{results:?}");
    assert!(results[0].1.starts_with(files::E_FILE_PATH), "{results:?}");
    assert!(
        !results[0].1.contains("s3-elsewhere"),
        "the refusal never names the path"
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn send_file_tool_rejects_home() {
    let mut h = Harness::new();
    // A moraebot home inside the work folder: the location rule alone must refuse it.
    h.config.home = h.config.work_dir.join("mb-home");
    let secret = h.work_file("mb-home/secrets/s3-token.txt");
    let manager = h.send_file_manager(&[secret.to_string_lossy().into_owned()]);
    let report = bounded(manager.prompt(KEY_A, 1, "hi")).await;
    assert_eq!(report.main.outcome, Outcome::Reply("echo: hi".into()));
    assert!(report.main.files.is_empty());
    let results = h.tool_results();
    assert_eq!(results.len(), 1);
    assert!(results[0].0, "{results:?}");
    assert!(results[0].1.starts_with(files::E_FILE_PATH), "{results:?}");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn send_file_tool_caps_files_per_turn() {
    let h = Harness::new();
    let paths: Vec<PathBuf> = (0..=files::MAX_FILES)
        .map(|n| h.work_file(&format!("s3-file-{n}.txt")))
        .collect();
    let lines: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let manager = h.send_file_manager(&lines);
    let report = bounded(manager.prompt(KEY_A, 1, "hi")).await;
    assert_eq!(report.main.outcome, Outcome::Reply("echo: hi".into()));
    assert_eq!(report.main.files, paths[..files::MAX_FILES]);
    let results = h.tool_results();
    assert_eq!(results.len(), files::MAX_FILES + 1);
    assert!(results[..files::MAX_FILES].iter().all(|(err, _)| !err));
    let (last_err, last_text) = &results[files::MAX_FILES];
    assert!(*last_err);
    assert!(last_text.starts_with(files::E_FILE_TOO_MANY), "{last_text}");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn send_file_bucket_is_per_turn() {
    let h = Harness::new();
    let a = h.work_file("s3-a.txt");
    let b = h.work_file("s3-b.txt");
    let manager = h.send_file_manager(&[
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    ]);
    let first = bounded(manager.prompt(KEY_A, 1, "one")).await;
    assert_eq!(first.main.files, [a.clone(), b.clone()]);
    let second = bounded(manager.prompt(KEY_A, 2, "two")).await;
    assert_eq!(second.main.outcome, Outcome::Reply("echo: two".into()));
    assert_eq!(
        second.main.files,
        [a, b],
        "only the second turn's own files"
    );
    assert_eq!(h.start_pids().len(), 1, "both turns ran on one child");
    manager.shutdown_all().await;
}

/// Shell stand-in for omp that runs fake-omp and drops its `prompt_result` frame, so a
/// turn never ends on its own. Written into the test's temp dir.
fn no_result_wrapper(root: &Path) -> PathBuf {
    let script = root.join("omp-no-result.sh");
    let fake = env!("CARGO_BIN_EXE_fake-omp");
    let body = format!(
        r#"#!/bin/sh
'{fake}' "$@" | awk '!/"type":"prompt_result"/ {{ print; fflush() }}'
"#
    );
    std::fs::write(&script, body).expect("write wrapper");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    script
}

#[tokio::test]
async fn files_dropped_when_turn_fails() {
    let mut h = Harness::new();
    h.config.omp_path = no_result_wrapper(h.dir.path());
    let file = h.work_file("s3-dropped.txt");
    let manager = h
        .send_file_manager(&[file.to_string_lossy().into_owned()])
        .with_turn_timeout(Duration::from_secs(5));
    let report = bounded(manager.prompt(KEY_A, 1, "hi")).await;
    assert!(
        matches!(report.main.outcome, Outcome::NeedsCheck { .. }),
        "{report:?}"
    );
    assert_eq!(report.main.outcome.code(), Some(omp::E_TURN_TIMEOUT));
    // The tool accepted the file during the turn (the fake logged the success result,
    // which the handler writes only after queueing), yet the report carries none.
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    assert!(report.main.files.is_empty());
    assert_eq!(manager.child_pid().await, None);
}

#[tokio::test]
async fn send_file_tool_never_runs_shell_payload() {
    let h = Harness::new();
    let manager = h.send_file_manager(&["$(touch /tmp/moraebot-pwned-3c1d)".to_owned()]);
    let report = bounded(manager.prompt(KEY_A, 1, "hi")).await;
    assert!(report.main.files.is_empty());
    let results = h.tool_results();
    assert_eq!(results.len(), 1);
    assert!(results[0].0);
    assert!(results[0].1.starts_with(files::E_FILE_PATH), "{results:?}");
    assert!(!Path::new("/tmp/moraebot-pwned-3c1d").exists());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn argv_unchanged_by_host_tools() {
    let h = Harness::new();
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, 1, "hi")).await;
    let start = h
        .log_lines()
        .into_iter()
        .find(|l| l.starts_with("START pid="))
        .expect("START line");
    assert_eq!(start_argv(&start), expected_argv(&h.config));
    let tools = h.frames("set_host_tools");
    assert_eq!(
        tools.len(),
        1,
        "the tool is registered once, right after startup"
    );
    assert_eq!(tools[0]["tools"][0]["name"], omp::SEND_FILE_TOOL);
    manager.shutdown_all().await;
}

// --- owner memory in the lead ----------------------------------------------------------

impl Harness {
    /// The memory home of this harness, with its folders and seeds made.
    fn memory(&self) -> MemoryHome {
        let home = MemoryHome::new(&self.config.home);
        home.ensure_layout().expect("memory layout");
        home
    }

    fn write_persona(&self, text: &str) {
        std::fs::write(
            self.config
                .home
                .join(memory::MEMORY_DIR)
                .join(memory::PERSONA),
            text,
        )
        .expect("write persona");
    }

    /// The `--append-system-prompt=` elements of every `START` line, in order.
    fn append_flags(&self) -> Vec<Vec<String>> {
        self.log_lines()
            .iter()
            .filter(|l| l.starts_with("START pid="))
            .map(|l| {
                start_argv(l)
                    .into_iter()
                    .filter(|a| a.starts_with("--append-system-prompt"))
                    .collect()
            })
            .collect()
    }

    /// `(is lane, tool names)` of every `set_host_tools` frame, by the child that got it.
    fn registered_tools(&self) -> Vec<(bool, Vec<String>)> {
        let mut lane = false;
        let mut out = Vec::new();
        for line in self.log_lines() {
            if line.starts_with("START pid=") {
                let argv = start_argv(&line);
                let at = argv.iter().position(|a| a == "--session-dir").unwrap();
                lane = argv[at + 1].contains("/lanes/");
            } else if let Ok(frame) = serde_json::from_str::<Value>(&line)
                && frame["type"] == "set_host_tools"
            {
                let names = frame["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t["name"].as_str().unwrap().to_owned())
                    .collect();
                out.push((lane, names));
            }
        }
        out
    }
}

fn persona_seed_body() -> &'static str {
    frontmatter::parse(PERSONA_SEED).expect("seed parses").body
}

#[tokio::test]
async fn lead_argv_ends_with_memory_block() {
    let h = Harness::new();
    let manager = h
        .manager("ok")
        .with_hints("probe hints".into())
        .with_memory(h.memory());
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    let flags = h.append_flags();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0].len(), 1, "one append element");
    let flag = &flags[0][0];
    assert!(flag.starts_with(&format!(
        "--append-system-prompt=probe hints\n\n{}",
        memory::BEGIN
    )));
    assert!(flag.ends_with(&format!("{}\n", memory::END)));
    assert!(flag.contains(persona_seed_body()));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn expired_file_and_notes_index_reach_the_lead() {
    let h = Harness::new();
    let home = h.memory();
    for (rel, text) in [
        (
            "system/old.md",
            "---\ndescription: d\nexpires: 2000-01-01\n---\nold-marker\n",
        ),
        (
            "system/new.md",
            "---\ndescription: d\nexpires: 2999-01-01\n---\nnew-marker\n",
        ),
        ("notes/n1.md", "---\ndescription: d\n---\nnote body\n"),
    ] {
        let path = h.memory_file(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("memory folder");
        std::fs::write(path, text).expect("write memory file");
    }
    let manager = h.manager("ok").with_memory(home);
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    let flags = h.append_flags();
    assert_eq!(flags.len(), 1);
    let flag = &flags[0][0];
    assert!(flag.contains("new-marker"));
    assert!(flag.contains("<external_projection>"));
    assert!(flag.contains("\nnotes/: n1\n"));
    assert!(!flag.contains("old-marker"));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn memory_block_is_rendered_at_each_start() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    bounded(manager.prompt(KEY_A, 1, "first")).await;
    let pid = manager.child_pid().await.expect("live child");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success());
    h.write_persona("---\ndescription: d\n---\nv2-marker\n");
    history::accept(&mut h.store.lock().unwrap(), &h.memory(), &[], "test")
        .expect("accept the hand edit");

    let lost = bounded(manager.prompt(KEY_A, 2, "second"))
        .await
        .main
        .outcome;
    assert_eq!(lost.code(), Some(omp::E_CHILD_LOST));
    let outcome = bounded(manager.prompt(KEY_A, 3, "third"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: third".into()));

    let flags = h.append_flags();
    assert_eq!(flags.len(), 2, "exactly two START lines");
    assert!(!flags[0][0].contains("v2-marker"));
    assert!(flags[1][0].contains("v2-marker"));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn hand_edit_is_not_projected() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    bounded(manager.prompt(KEY_A, 1, "first")).await;
    let pid = manager.child_pid().await.expect("live child");
    h.write_persona("---\ndescription: d\n---\nhand-marker\n");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success());

    let lost = bounded(manager.prompt(KEY_A, 2, "second"))
        .await
        .main
        .outcome;
    assert_eq!(lost.code(), Some(omp::E_CHILD_LOST));
    assert_eq!(
        h.append_flags().len(),
        1,
        "a lost child starts no new child"
    );
    let outcome = bounded(manager.prompt(KEY_A, 3, "third"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: third".into()));

    let flags = h.append_flags();
    assert_eq!(flags.len(), 2, "exactly two START lines");
    let second = &flags[1][0];
    assert!(!second.contains("hand-marker"));
    assert!(second.contains(persona_seed_body()));
    assert!(second.contains(&format!(
        "<dirty path=\"{}\" code=\"{}\"/>",
        memory::PERSONA,
        memory::E_MEMORY_DIRTY
    )));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn unusable_memory_file_does_not_block_the_lead() {
    let h = Harness::new();
    let home = h.memory();
    h.write_persona("no frontmatter here\n");
    let manager = h.manager("ok").with_memory(home);
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    let flags = h.append_flags();
    assert!(flags[0][0].contains(&format!(
        "path=\"{}\" code=\"{}\"",
        memory::PERSONA,
        memory::E_MEMORY_FRONTMATTER
    )));
    assert!(!flags[0][0].contains("no frontmatter here"));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn lead_registers_memory_tools_lanes_do_not() {
    let h = Harness::new();
    let call = serde_json::json!({
        "tool": lane::LANE_START_TOOL,
        "arguments": {"task": "t1", "done": "file ."}
    });
    let manager = h
        .manager("tools")
        .with_extra_env([
            ("FAKE_OMP_LANE_MODE", "ok".to_owned()),
            ("FAKE_OMP_TOOL_CALLS", call.to_string()),
        ])
        .with_memory(h.memory());
    let wake = manager.lane_wake();
    let notified = wake.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    bounded(notified).await;

    let registered = h.registered_tools();
    assert_eq!(registered.len(), 2, "{registered:?}");
    let (lead_frame_is_lane, lead_names) = &registered[0];
    assert!(!lead_frame_is_lane);
    assert_eq!(lead_names[0], omp::SEND_FILE_TOOL, "{lead_names:?}");
    assert_eq!(&lead_names[1..9], &MEMORY_TOOLS, "{lead_names:?}");
    assert_eq!(&lead_names[9..11], &AGREEMENT_TOOLS, "{lead_names:?}");
    assert_eq!(
        lead_names[11],
        memory::ONBOARDING_FINISH_TOOL,
        "{lead_names:?}"
    );
    let (lane_frame_is_lane, lane_names) = &registered[1];
    assert!(lane_frame_is_lane);
    assert_eq!(lane_names, &[done::LANE_DONE_TOOL]);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn memory_read_round_trip_through_the_lead() {
    let h = Harness::new();
    let calls = [
        serde_json::json!({"tool": memory::MEMORY_READ_TOOL, "arguments": {"path": memory::PERSONA}}),
        serde_json::json!({"tool": memory::MEMORY_READ_TOOL, "arguments": {"path": "../x.md"}}),
    ]
    .map(|c| c.to_string())
    .join("\n");
    let manager = h
        .manager("tools")
        .with_extra_env([("FAKE_OMP_TOOL_CALLS", calls)])
        .with_memory(h.memory());
    let outcome = bounded(manager.prompt(KEY_A, 1, "run-tools"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: run-tools".into()));

    let results = h.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    let read: Value = serde_json::from_str(&results[0].1).expect("memory_read json");
    assert_eq!(read["body"], persona_seed_body());
    assert!(results[1].0, "{results:?}");
    assert!(
        results[1]
            .1
            .starts_with(&format!("{}:", memory::E_MEMORY_PATH)),
        "{results:?}"
    );
    manager.shutdown_all().await;
}

/// Every memory tool, in registration order.
const MEMORY_TOOLS: [&str; 8] = [
    memory::MEMORY_LIST_TOOL,
    memory::MEMORY_READ_TOOL,
    memory::MEMORY_CREATE_TOOL,
    memory::MEMORY_STR_REPLACE_TOOL,
    memory::MEMORY_INSERT_TOOL,
    memory::MEMORY_DELETE_TOOL,
    memory::MEMORY_RENAME_TOOL,
    memory::MEMORY_UPDATE_DESCRIPTION_TOOL,
];

/// The agreement tools, registered right after the memory tools.
const AGREEMENT_TOOLS: [&str; 2] = [
    memory::MEMORY_AGREEMENT_RECORD_TOOL,
    memory::MEMORY_AGREEMENT_BREAK_TOOL,
];

impl Harness {
    /// A `tools`-mode manager with memory whose fake lead makes `calls` on a `run-tools` prompt.
    fn memory_tools_manager(&self, calls: &[Value]) -> OmpManager {
        let calls = calls
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        self.manager("tools")
            .with_extra_env([("FAKE_OMP_TOOL_CALLS", calls)])
            .with_memory(self.memory())
    }

    fn memory_commits(&self) -> Vec<moraebot::store::MemoryCommitRow> {
        self.store
            .lock()
            .unwrap()
            .memory_commits()
            .expect("commits")
    }

    fn memory_file(&self, rel: &str) -> PathBuf {
        self.config.home.join(memory::MEMORY_DIR).join(rel)
    }
}

fn tool_call(tool: &str, arguments: Value) -> Value {
    serde_json::json!({"tool": tool, "arguments": arguments})
}

fn create_call(path: &str) -> Value {
    tool_call(
        memory::MEMORY_CREATE_TOOL,
        serde_json::json!({"path": path, "description": "about a", "body": "line one\n", "reason": "r"}),
    )
}

fn refused_with(result: &(bool, String), code: &str) -> bool {
    result.0 && result.1.starts_with(&format!("{code}:"))
}

#[tokio::test]
async fn memory_write_round_trip_through_the_lead() {
    let h = Harness::new();
    let calls = [
        create_call("notes/a.md"),
        tool_call(
            memory::MEMORY_STR_REPLACE_TOOL,
            serde_json::json!({"path": "notes/a.md", "old_string": "line one", "new_string": "line two", "reason": "r"}),
        ),
        tool_call(
            memory::MEMORY_INSERT_TOOL,
            serde_json::json!({"path": "notes/a.md", "insert_line": 1, "insert_text": "top", "reason": "r"}),
        ),
        tool_call(
            memory::MEMORY_UPDATE_DESCRIPTION_TOOL,
            serde_json::json!({"path": "notes/a.md", "description": "about b", "reason": "r"}),
        ),
        tool_call(
            memory::MEMORY_RENAME_TOOL,
            serde_json::json!({"old_path": "notes/a.md", "new_path": "notes/b.md", "reason": "r"}),
        ),
        tool_call(
            memory::MEMORY_DELETE_TOOL,
            serde_json::json!({"path": "notes/b.md", "reason": "r"}),
        ),
    ];
    let manager = h.memory_tools_manager(&calls);
    let outcome = bounded(manager.prompt(KEY_A, 1, "run-tools"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: run-tools".into()));

    let results = h.tool_results();
    assert_eq!(results.len(), 6, "{results:?}");
    let mut last = 0;
    for ((is_error, text), call) in results.iter().zip(&calls) {
        assert!(!is_error, "{results:?}");
        let value: Value = serde_json::from_str(text).expect("write result json");
        assert_eq!(value["command"], call["tool"], "{value}");
        let committed = value["committed"].as_i64().expect("committed id");
        assert!(committed > last, "{results:?}");
        last = committed;
    }
    let renamed: Value = serde_json::from_str(&results[4].1).unwrap();
    assert_eq!(renamed["to"], "notes/b.md");
    assert!(!h.memory_file("notes/a.md").exists());
    assert!(!h.memory_file("notes/b.md").exists());

    let commits = h.memory_commits();
    assert_eq!(commits.len(), 7, "the import and six writes");
    assert_eq!(commits[0].commit.writer, memory::WRITER_IMPORT);
    for row in &commits[1..] {
        assert_eq!(row.commit.writer, memory::WRITER_LEAD);
        assert_eq!(row.commit.turn_row, Some(1));
        assert_eq!(row.commit.conversation_key.as_deref(), Some(KEY_A));
        assert!(row.applied);
    }
    manager.shutdown_all().await;
}

/// Stores a lead reply `proposal-text` in `KEY_A`, then the owner's answer `text`; returns both row ids.
fn proposal_then_answer(h: &Harness, text: &str) -> (i64, i64) {
    let mut store = h.store.lock().unwrap();
    let admit = |store: &mut Store, message_id: &str, text: &str| match store
        .admit("discord", message_id, KEY_A, "222", "111", text)
        .expect("admit")
    {
        moraebot::store::Admitted::New(id) => id,
        other => panic!("expected a new row, got {other:?}"),
    };
    let p = admit(&mut store, "p1", "please remember tea");
    assert!(store.claim(p).expect("claim"));
    store
        .finish_group(p, "proposal-text", None, None)
        .expect("finish group");
    let r = admit(&mut store, "r1", text);
    (p, r)
}

fn approved_create_call(r: i64) -> Value {
    tool_call(
        memory::MEMORY_CREATE_TOOL,
        serde_json::json!({
            "path": "system/approved.md",
            "description": "approved",
            "body": "green tea\n",
            "reason": "r",
            "source": "user",
            "user_quote": "yes",
            "quote_message": r,
        }),
    )
}

fn file_provenance(h: &Harness, rel: &str) -> String {
    let text = std::fs::read_to_string(h.memory_file(rel)).expect("memory file");
    let parsed = frontmatter::parse(&text).expect("file parses");
    parsed
        .frontmatter
        .get(memory::PROVENANCE_KEY)
        .expect("provenance")
        .to_owned()
}

#[tokio::test]
async fn owner_approval_is_recorded_through_the_lead() {
    let h = Harness::new();
    let (p, r) = proposal_then_answer(&h, "yes");
    let manager = h.memory_tools_manager(&[approved_create_call(r)]);
    bounded(manager.prompt(KEY_A, r, "run-tools")).await;

    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    let value: Value = serde_json::from_str(&results[0].1).expect("write result json");
    assert_eq!(value["source"], "user", "{value}");
    assert_eq!(value["downgraded"], false, "{value}");

    let commits = h.memory_commits();
    let last = commits.last().expect("a commit");
    assert_eq!(last.commit.writer, memory::WRITER_LEAD);
    let claim: Value = serde_json::from_str(last.commit.claim_json.as_deref().expect("claim json"))
        .expect("claim parses");
    assert_eq!(claim["source"], "user", "{claim}");
    assert_eq!(claim["message"], r, "{claim}");
    assert_eq!(claim["proposal_row"], p, "{claim}");
    assert_eq!(
        claim["proposal_hash"],
        memory::provenance::proposal_hash("proposal-text"),
        "{claim}"
    );
    assert_eq!(claim["platform"], "discord", "{claim}");
    assert_eq!(claim["message_id"], "r1", "{claim}");
    assert!(file_provenance(&h, "system/approved.md").starts_with("user:"));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn unverified_claim_is_downgraded_through_the_lead() {
    let h = Harness::new();
    let (_, r) = proposal_then_answer(&h, "maybe later");
    let manager = h.memory_tools_manager(&[approved_create_call(r)]);
    bounded(manager.prompt(KEY_A, r, "run-tools")).await;

    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    let value: Value = serde_json::from_str(&results[0].1).expect("write result json");
    assert_eq!(value["source"], "agent", "{value}");
    assert_eq!(value["downgraded"], true, "{value}");
    assert_eq!(value["code"], memory::E_MEMORY_QUOTE_TEXT, "{value}");
    assert!(h.memory_file("system/approved.md").exists());
    assert!(file_provenance(&h, "system/approved.md").starts_with("agent:"));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn bad_claim_arguments_are_refused_through_the_lead() {
    let h = Harness::new();
    let calls = [
        tool_call(
            memory::MEMORY_CREATE_TOOL,
            serde_json::json!({"path": "notes/a.md", "description": "about a", "body": "line one\n", "reason": "r", "source": "owner"}),
        ),
        tool_call(
            memory::MEMORY_CREATE_TOOL,
            serde_json::json!({"path": "notes/a.md", "description": "about a", "body": "line one\n", "reason": "r", "quote_message": 3}),
        ),
    ];
    let manager = h.memory_tools_manager(&calls);
    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;

    let results = h.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    for result in &results {
        assert!(refused_with(result, memory::E_MEMORY_ARGS), "{results:?}");
    }
    let commits = h.memory_commits();
    assert_eq!(commits.len(), 1, "the import only");
    assert_eq!(commits[0].commit.writer, memory::WRITER_IMPORT);
    assert!(!h.memory_file("notes/a.md").exists());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn write_during_lane_result_turn_is_refused() {
    let h = Harness::new();
    let lane_row = {
        let mut store = h.store.lock().unwrap();
        let lane = match store
            .start_lane("toolu_1", KEY_A, "t", None, lane::MAX_RUNNING_LANES)
            .expect("start lane")
        {
            LaneStart::New(id) => id,
            other => panic!("expected a new lane, got {other:?}"),
        };
        assert!(
            store
                .finish_lane(lane, 1, "lane result", LaneState::Idle, None)
                .expect("finish lane")
        );
        let row = store.get(3).expect("get").expect("lane row 3");
        assert_eq!(row.origin, "lane");
        row.id
    };
    assert_eq!(lane_row, 3);

    let manager = h.memory_tools_manager(&[create_call("notes/a.md")]);
    bounded(manager.prompt(KEY_A, lane_row, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        refused_with(&results[0], memory::E_MEMORY_ORIGIN),
        "{results:?}"
    );
    assert!(!h.memory_file("notes/a.md").exists());
    let commits = h.memory_commits();
    assert_eq!(commits.len(), 1, "only the import");
    assert_eq!(commits[0].commit.writer, memory::WRITER_IMPORT);

    // Control: the same call during an owner turn is recorded.
    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(!results[1].0, "{results:?}");
    assert!(h.memory_file("notes/a.md").exists());
    assert_eq!(h.memory_commits().len(), 2);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn committed_write_shows_at_next_start() {
    let h = Harness::new();
    let manager = h.memory_tools_manager(&[tool_call(
        memory::MEMORY_STR_REPLACE_TOOL,
        serde_json::json!({
            "path": memory::PERSONA,
            "old_string": persona_seed_body(),
            "new_string": "tool-marker\n",
            "reason": "r"
        }),
    )]);
    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    let pid = manager.child_pid().await.expect("live child");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success());

    let lost = bounded(manager.prompt(KEY_A, 2, "second"))
        .await
        .main
        .outcome;
    assert_eq!(lost.code(), Some(omp::E_CHILD_LOST));
    assert_eq!(
        h.append_flags().len(),
        1,
        "a lost child starts no new child"
    );
    let outcome = bounded(manager.prompt(KEY_A, 3, "third"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: third".into()));

    let flags = h.append_flags();
    assert_eq!(flags.len(), 2, "exactly two START lines");
    assert!(!flags[0][0].contains("tool-marker"));
    assert!(flags[1][0].contains("tool-marker"));
    assert!(!flags[1][0].contains("<dirty path="));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn hand_edit_blocks_memory_writes() {
    let h = Harness::new();
    let manager = h.memory_tools_manager(&[
        create_call("notes/a.md"),
        tool_call(
            memory::MEMORY_READ_TOOL,
            serde_json::json!({"path": memory::HUMAN}),
        ),
    ]);
    bounded(manager.prompt(KEY_A, 1, "import")).await;
    assert_eq!(h.memory_commits().len(), 1, "the import");
    std::fs::write(
        h.memory_file(memory::HUMAN),
        "---\ndescription: d\n---\nhand-marker\n",
    )
    .expect("hand edit");

    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        refused_with(&results[0], memory::E_MEMORY_DIRTY),
        "{results:?}"
    );
    assert!(!results[1].0, "{results:?}");
    let read: Value = serde_json::from_str(&results[1].1).expect("memory_read json");
    assert_eq!(read["body"], "hand-marker\n");
    assert!(!h.memory_file("notes/a.md").exists());
    assert_eq!(h.memory_commits().len(), 1, "no commit was added");
    manager.shutdown_all().await;
}

/// A secret-like value built at run time: `parts` joined, then `n` copies of `fill`, so no
/// source line holds a whole token.
fn fake(parts: &[&str], fill: char, n: usize) -> String {
    let mut out = parts.concat();
    out.extend(std::iter::repeat_n(fill, n));
    out
}

#[tokio::test]
async fn read_only_write_is_refused_through_the_lead() {
    let h = Harness::new();
    let ro = h.memory_file("notes/ro.md");
    std::fs::create_dir_all(ro.parent().expect("parent")).expect("notes folder");
    let text = "---\ndescription: d\nread_only: \"true\"\n---\nro-marker\n";
    std::fs::write(&ro, text).expect("write read_only file");
    let manager = h.memory_tools_manager(&[tool_call(
        memory::MEMORY_STR_REPLACE_TOOL,
        serde_json::json!({"path": "notes/ro.md", "old_string": "ro-marker", "new_string": "changed", "reason": "r"}),
    )]);
    bounded(manager.prompt(KEY_A, 1, "import")).await;
    assert_eq!(h.memory_commits().len(), 1, "the import");

    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        refused_with(&results[0], memory::E_MEMORY_READ_ONLY),
        "{results:?}"
    );
    assert_eq!(std::fs::read(&ro).expect("read_only file"), text.as_bytes());
    assert_eq!(h.memory_commits().len(), 1, "no commit was added");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn secret_write_is_refused_through_the_lead() {
    let h = Harness::new();
    let key = fake(&["sk", "-"], 'a', 24);
    let manager = h.memory_tools_manager(&[tool_call(
        memory::MEMORY_CREATE_TOOL,
        serde_json::json!({
            "path": "notes/k.md",
            "description": "d",
            "body": format!("key {key}\n"),
            "reason": "r"
        }),
    )]);
    bounded(manager.prompt(KEY_A, 1, "import")).await;
    assert_eq!(h.memory_commits().len(), 1, "the import");

    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "tool result count");
    assert!(
        refused_with(&results[0], memory::E_MEMORY_SECRET),
        "not refused with the secret code"
    );
    assert!(!results[0].1.contains(&key), "refusal echoes the value");
    assert!(!h.memory_file("notes/k.md").exists());
    assert_eq!(h.memory_commits().len(), 1, "no commit was added");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn mis_closed_call_is_repaired_through_the_lead() {
    let h = Harness::new();
    let manager = h.memory_tools_manager(&[tool_call(
        memory::MEMORY_STR_REPLACE_TOOL,
        serde_json::json!({
            "path": memory::PERSONA,
            "old_string": format!(
                "{}</old_string>\n<parameter name=\"new_string\">repair-marker\n",
                persona_seed_body()
            ),
            "reason": "r"
        }),
    )]);
    bounded(manager.prompt(KEY_A, 1, "import")).await;
    assert_eq!(h.memory_commits().len(), 1, "the import");

    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    let value: Value = serde_json::from_str(&results[0].1).expect("write result json");
    assert_eq!(
        value["repaired"],
        serde_json::json!(["new_string"]),
        "{value}"
    );
    let persona = std::fs::read_to_string(h.memory_file(memory::PERSONA)).expect("persona");
    let body = frontmatter::parse(&persona).expect("persona parses").body;
    assert_eq!(body, "repair-marker\n");
    assert_eq!(h.memory_commits().len(), 2, "the import and the write");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn no_memory_means_unchanged_argv_and_tools() {
    let h = Harness::new();
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, 1, "hi")).await;
    let start = h
        .log_lines()
        .into_iter()
        .find(|l| l.starts_with("START pid="))
        .expect("START line");
    assert_eq!(start_argv(&start), expected_argv(&h.config));
    let registered = h.registered_tools();
    assert_eq!(registered.len(), 1);
    assert_eq!(
        registered[0].1,
        [
            omp::SEND_FILE_TOOL,
            lane::LANE_START_TOOL,
            lane::LANE_MESSAGE_TOOL,
            lane::LANE_STATUS_TOOL,
        ]
    );
    manager.shutdown_all().await;
}

// --- lead turns: follow-up, steer, turn context --------------------------------------

const KEY_STEER: &str = "discord:th:111";

impl Harness {
    /// A shared `steer`-mode manager whose fake answers a steer by `case`.
    fn steer_manager(&self, case: &str) -> Arc<OmpManager> {
        Arc::new(
            self.manager("steer")
                .with_extra_env([("FAKE_OMP_STEER_CASE", case)]),
        )
    }

    /// Prompt frames whose `streamingBehavior` is `steer`.
    fn steer_frames(&self) -> Vec<Value> {
        self.frames("prompt")
            .into_iter()
            .filter(|f| f["streamingBehavior"] == "steer")
            .collect()
    }

    fn admit(&self, message_id: &str, text: &str) -> i64 {
        match self
            .store
            .lock()
            .unwrap()
            .admit("discord", message_id, KEY_STEER, "222", "111", text)
            .expect("admit")
        {
            moraebot::store::Admitted::New(id) => id,
            moraebot::store::Admitted::Duplicate => panic!("duplicate message id"),
        }
    }
}

/// Starts the main prompt on its own task.
fn spawn_prompt(
    manager: &Arc<OmpManager>,
    key: &'static str,
    row_id: i64,
    text: &'static str,
) -> tokio::task::JoinHandle<GroupReport> {
    let manager = Arc::clone(manager);
    tokio::spawn(async move { manager.prompt(key, row_id, text).await })
}

/// Waits until the lead reported `agent_start` for a turn of `key`.
async fn turn_running(manager: &OmpManager, key: &str) {
    let mut running = manager.running_turn();
    bounded(running.wait_for(|k| k.as_deref() == Some(key)))
        .await
        .expect("running-turn watch open");
}

fn codes(report: &GroupReport) -> (Option<&'static str>, Option<(i64, Option<&'static str>)>) {
    (
        report.main.outcome.code(),
        report
            .steered
            .as_ref()
            .map(|(id, outcome)| (*id, outcome.code())),
    )
}

#[tokio::test]
async fn main_prompt_uses_follow_up() {
    let h = Harness::new();
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, 1, "hi")).await;
    let prompts = h.frames("prompt");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["streamingBehavior"], "followUp");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn steer_merged_gives_both_rows_one_text() {
    let h = Harness::new();
    let manager = h.steer_manager("merged");
    let turn = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;
    assert!(manager.try_steer(KEY_STEER, 2, "m2", |_| true));
    let report = bounded(turn).await.expect("prompt task");
    assert_eq!(report.main.outcome, Outcome::Reply("echo: m1 + m2".into()));
    assert_eq!(
        report.steered,
        Some((2, Outcome::Reply("echo: m1 + m2".into())))
    );
    let steers = h.steer_frames();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["message"], "m2");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn steer_separate_gives_each_row_its_text() {
    let h = Harness::new();
    let manager = h.steer_manager("separate");
    let turn = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;
    assert!(manager.try_steer(KEY_STEER, 2, "m2", |_| true));
    let report = bounded(turn).await.expect("prompt task");
    assert_eq!(report.main.outcome, Outcome::Reply("echo: m1".into()));
    assert_eq!(report.steered, Some((2, Outcome::Reply("echo: m2".into()))));
    assert_eq!(h.steer_frames().len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn steer_refused_for_other_conversation_or_closed_turn() {
    let h = Harness::new();
    let manager = h.steer_manager("merged");
    let turn = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;

    let mut called = false;
    assert!(!manager.try_steer("discord:th:999", 2, "other", |_| {
        called = true;
        true
    }));
    assert!(!called, "another conversation never reaches the claim");

    assert!(manager.try_steer(KEY_STEER, 3, "m2", |_| true));
    let report = bounded(turn).await.expect("prompt task");
    assert_eq!(report.steered.map(|(id, _)| id), Some(3));

    assert!(!manager.try_steer(KEY_STEER, 4, "late", |_| {
        called = true;
        true
    }));
    assert!(!called, "a closed turn never reaches the claim");
    assert_eq!(*manager.running_turn().borrow(), None);
    assert_eq!(h.steer_frames().len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn turn_context_is_set_during_the_turn() {
    let h = Harness::new();
    let base = h.manager("tools").with_extra_env([(
        "FAKE_OMP_TOOL_CALLS",
        r#"{"tool": "ctx_probe", "arguments": {}}"#,
    )]);
    let context = base.turn_context();
    let handler_context = Arc::clone(&context);
    let parameters = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }))
    .expect("schema object");
    let probe = omp_rpc::HostTool::new("ctx_probe", "test probe", parameters, move |_, _| {
        let ctx = handler_context.lock().unwrap().clone();
        let text = match ctx {
            Some(ctx) => serde_json::json!({"row": ctx.row_id, "key": ctx.conversation_key}),
            None => serde_json::json!(null),
        };
        Ok(text.to_string().into())
    });
    let manager = base.with_host_tools(vec![probe]);

    let report = bounded(manager.prompt(KEY_STEER, 7, "run-tools ctx")).await;
    assert_eq!(
        report.main.outcome,
        Outcome::Reply("echo: run-tools ctx".into())
    );
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    let seen: Value = serde_json::from_str(&results[0].1).expect("probe json");
    assert_eq!(seen, serde_json::json!({"row": 7, "key": KEY_STEER}));
    let tools = h.frames("set_host_tools");
    assert_eq!(tools[0]["tools"][0]["name"], omp::SEND_FILE_TOOL);
    assert_eq!(tools[0]["tools"][1]["name"], "ctx_probe");
    assert_eq!(*context.lock().unwrap(), None, "cleared after the turn");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn steer_claim_race_runs_row_once() {
    let h = Harness::new();
    let a = h.admit("race-a", "mA");
    let r = h.admit("race-r", "mR");
    let s = h.admit("race-s", "mS");
    {
        let store = h.store.lock().unwrap();
        assert!(store.claim(a).unwrap());
        assert!(store.claim(r).unwrap(), "the worker took R first");
    }
    let manager = h.steer_manager("merged");
    let turn = spawn_prompt(&manager, KEY_STEER, a, "mA");
    turn_running(&manager, KEY_STEER).await;

    let mut called = None;
    let steered_r = manager.try_steer(KEY_STEER, r, "mR", |main| {
        called = Some(main);
        h.store.lock().unwrap().steer_into(r, main).unwrap()
    });
    assert!(!steered_r);
    assert_eq!(called, Some(a), "the claim got the open turn's main row");

    let steered_s = manager.try_steer(KEY_STEER, s, "mS", |main| {
        h.store.lock().unwrap().steer_into(s, main).unwrap()
    });
    assert!(steered_s);
    let report = bounded(turn).await.expect("prompt task");
    assert_eq!(
        report.steered,
        Some((s, Outcome::Reply("echo: mA + mS".into())))
    );

    let steers = h.steer_frames();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["message"], "mS");
    assert!(h.frames("prompt").iter().all(|f| f["message"] != "mR"));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn shutdown_all_does_not_wait_for_the_running_turn() {
    let h = Harness::new();
    let manager = h.steer_manager("hold");
    let turn = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;
    assert!(manager.try_steer(KEY_STEER, 2, "m2", |_| true));

    bounded(manager.shutdown_all()).await;
    let report = bounded(turn).await.expect("prompt task");
    assert!(matches!(report.main.outcome, Outcome::NeedsCheck { .. }));
    assert!(matches!(
        report.steered,
        Some((2, Outcome::NeedsCheck { .. }))
    ));
    assert_eq!(
        codes(&report),
        (Some(omp::E_CHILD_LOST), Some((2, Some(omp::E_CHILD_LOST))))
    );
    assert_eq!(manager.child_pid().await, None);
    assert!(!alive(h.start_pids()[0]));
}

/// The dispatcher drops the running `prompt` at shutdown. `shutdown_all` must then still
/// join the lead worker promptly, not after the `ABORT_JOIN_BOUND` fallback.
#[tokio::test]
async fn shutdown_all_after_a_cancelled_turn_joins_the_lead_promptly() {
    let h = Harness::new();
    let manager = h.steer_manager("hold");
    let turn = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;
    assert!(manager.try_steer(KEY_STEER, 2, "m2", |_| true));
    turn.abort();
    assert!(bounded(turn).await.expect_err("aborted").is_cancelled());

    tokio::time::timeout(omp::ABORT_JOIN_BOUND / 10, manager.shutdown_all())
        .await
        .expect("shutdown_all joins the lead worker well before ABORT_JOIN_BOUND");
    assert_eq!(manager.child_pid().await, None);
    assert!(!alive(h.start_pids()[0]));
}

#[tokio::test]
async fn second_steer_in_one_turn_is_refused() {
    let h = Harness::new();
    let manager = h.steer_manager("hold");
    let turn = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;
    assert!(manager.try_steer(KEY_STEER, 2, "m2", |_| true));

    let mut called = false;
    assert!(!manager.try_steer(KEY_STEER, 3, "m3", |_| {
        called = true;
        true
    }));
    assert!(!called, "a turn that holds a steer never reaches the claim");

    bounded(manager.shutdown_all()).await;
    let report = bounded(turn).await.expect("prompt task");
    assert_eq!(
        codes(&report),
        (Some(omp::E_CHILD_LOST), Some((2, Some(omp::E_CHILD_LOST))))
    );
    // The kill may land before the steer frame is written, so at most one steer frame.
    let steers = h.steer_frames();
    assert!(steers.len() <= 1, "{steers:?}");
    assert!(h.frames("prompt").iter().all(|f| f["message"] != "m3"));
}

#[tokio::test]
async fn turn_timeout_with_steer_kills_lead_before_waiting() {
    let h = Harness::new();
    let manager = Arc::new(
        h.manager("steer")
            .with_extra_env([("FAKE_OMP_STEER_CASE", "hold")])
            .with_turn_timeout(Duration::from_secs(1)),
    );
    let turn = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;
    assert!(manager.try_steer(KEY_STEER, 2, "m2", |_| true));

    let report = bounded(turn).await.expect("prompt task");
    assert_eq!(
        codes(&report),
        (
            Some(omp::E_TURN_TIMEOUT),
            Some((2, Some(omp::E_CHILD_LOST)))
        )
    );
    let pids = h.start_pids();
    assert_eq!(pids.len(), 1);
    assert!(!alive(pids[0]), "the lead child is gone");
    assert_eq!(manager.child_pid().await, None);
}

// --- memory pin and the restart gate ----------------------------------------------------

const COMPACT: (&str, &str) = ("FAKE_OMP_COMPACT_TRIGGER", "compact-now");

impl Harness {
    /// A shared memory `steer`-mode manager whose fake answers a steer by `case`.
    fn steer_memory_manager(&self, case: &str) -> Arc<OmpManager> {
        Arc::new(
            self.manager("steer")
                .with_extra_env([("FAKE_OMP_STEER_CASE", case)])
                .with_memory(self.memory()),
        )
    }

    /// Commits a hand edit of the persona whose body is `marker`.
    fn change_persona(&self, marker: &str) {
        self.write_persona(&format!("---\ndescription: d\n---\n{marker}\n"));
        history::accept(&mut self.store.lock().unwrap(), &self.memory(), &[], "test")
            .expect("accept the hand edit");
    }

    /// The `--session-dir` value of every `START` line, in order.
    fn session_dirs(&self) -> Vec<String> {
        self.log_lines()
            .iter()
            .filter(|l| l.starts_with("START pid="))
            .map(|l| {
                let argv = start_argv(l);
                let at = argv.iter().position(|a| a == "--session-dir").unwrap();
                argv[at + 1].clone()
            })
            .collect()
    }
}

/// A clock the test steps by hand.
fn stepped_clock(start: i64) -> (Arc<AtomicI64>, notice::Clock) {
    let now = Arc::new(AtomicI64::new(start));
    let read = Arc::clone(&now);
    (now, Arc::new(move || read.load(Ordering::SeqCst)))
}

#[tokio::test]
async fn write_alone_does_not_restart_the_lead() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    bounded(manager.prompt(KEY_A, 1, "first")).await;
    h.change_persona("v2-marker");
    let outcome = bounded(manager.prompt(KEY_A, 2, "second"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: second".into()));
    assert_eq!(h.append_flags().len(), 1);
    manager.shutdown_all().await;
}

/// Adds one `global` agent agreement straight to the ledger, as recorded in owner turn 1.
fn add_agreement(h: &Harness, text: &str) -> i64 {
    let norm = text.to_lowercase();
    let new = NewAgreement {
        scope: "global",
        author: "agent",
        text,
        norm: &norm,
        turn_row: 1,
        reason: "test",
        claim_json: None,
    };
    match h
        .store
        .lock()
        .unwrap()
        .add_agreement(&new, 64, 12288)
        .expect("add agreement")
    {
        AgreementAdded::Added(id) => id,
        other => panic!("agreement not added: {other:?}"),
    }
}

#[tokio::test]
async fn agreements_reach_the_lead_block() {
    let h = Harness::new();
    assert_eq!(add_agreement(&h, "never force push"), 1);
    let manager = h.manager("ok").with_memory(h.memory());
    let outcome = bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));
    let flags = h.append_flags();
    assert_eq!(flags.len(), 1);
    assert!(flags[0][0].contains("<agreements>\n"));
    assert!(
        flags[0][0]
            .lines()
            .any(|line| line == "- #1 [global] [agent] never force push")
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn agreement_change_alone_does_not_restart() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    bounded(manager.prompt(KEY_A, 1, "first")).await;
    assert_eq!(add_agreement(&h, "ask before deploys"), 1);
    let outcome = bounded(manager.prompt(KEY_A, 2, "second"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: second".into()));
    assert_eq!(h.append_flags().len(), 1, "exactly one START line");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn compaction_with_changed_memory_restarts_before_next_prompt() {
    let h = Harness::new();
    let manager = h
        .manager("ok")
        .with_extra_env([COMPACT])
        .with_memory(h.memory());
    bounded(manager.prompt(KEY_A, 1, "compact-now")).await;
    h.change_persona("v2-marker");
    let outcome = bounded(manager.prompt(KEY_A, 2, "hi")).await.main.outcome;
    assert_eq!(outcome, Outcome::Reply("echo: hi".into()));

    let flags = h.append_flags();
    assert_eq!(flags.len(), 2);
    assert!(!flags[0][0].contains("v2-marker"));
    assert!(flags[1][0].contains("v2-marker"));
    let dirs = h.session_dirs();
    assert_eq!(dirs[0], dirs[1]);
    let tools = h.registered_tools();
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().all(|(lane, _)| !lane));
    assert_eq!(tools[0].1, tools[1].1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn compaction_without_change_keeps_the_child() {
    let h = Harness::new();
    let manager = h
        .manager("ok")
        .with_extra_env([COMPACT])
        .with_memory(h.memory());
    bounded(manager.prompt(KEY_A, 1, "compact-now")).await;
    bounded(manager.prompt(KEY_A, 2, "hi")).await;
    h.change_persona("v2-marker");
    let outcome = bounded(manager.prompt(KEY_A, 3, "again"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: again".into()));
    assert_eq!(h.append_flags().len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn no_restart_before_session_settled() {
    let h = Harness::new();
    let manager = h
        .manager("ok")
        .with_extra_env([COMPACT, ("FAKE_OMP_UNSETTLED_TRIGGER", "hold-settle")])
        .with_memory(h.memory());
    bounded(manager.prompt(KEY_A, 1, "compact-now hold-settle")).await;
    h.change_persona("v2-marker");
    bounded(manager.prompt(KEY_A, 2, "hi")).await;
    assert_eq!(
        h.append_flags().len(),
        1,
        "deferred: the lead was not settled"
    );
    bounded(manager.prompt(KEY_A, 3, "again")).await;
    assert_eq!(h.append_flags().len(), 2, "prompt 2 settled the lead");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn session_settled_frame_allows_the_restart() {
    let h = Harness::new();
    let manager = h
        .manager("ok")
        .with_extra_env([
            COMPACT,
            ("FAKE_OMP_UNSETTLED_TRIGGER", "hold-settle"),
            ("FAKE_OMP_SETTLE_FRAME", "1"),
        ])
        .with_memory(h.memory());
    bounded(manager.prompt(KEY_A, 1, "compact-now hold-settle")).await;
    let mut state = manager.lead_state();
    bounded(state.wait_for(|s| s.results >= 1 && s.settled))
        .await
        .expect("lead-state watch open");
    h.change_persona("v2-marker");
    bounded(manager.prompt(KEY_A, 2, "hi")).await;
    assert_eq!(h.append_flags().len(), 2);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn aborted_or_skipped_compaction_is_not_counted() {
    for (kind, starts) in [("aborted", 1), ("skipped", 1), ("manual", 2)] {
        let h = Harness::new();
        let manager = h
            .manager("ok")
            .with_extra_env([COMPACT, ("FAKE_OMP_COMPACT", kind)])
            .with_memory(h.memory());
        bounded(manager.prompt(KEY_A, 1, "compact-now")).await;
        h.change_persona("v2-marker");
        bounded(manager.prompt(KEY_A, 2, "hi")).await;
        assert_eq!(h.append_flags().len(), starts, "FAKE_OMP_COMPACT={kind}");
        manager.shutdown_all().await;
    }
}

#[tokio::test]
async fn refresh_request_restarts_without_change() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    let refresh = notice::refresh_file(&h.config.home);
    bounded(manager.prompt(KEY_A, 1, "first")).await;
    std::fs::write(&refresh, "").expect("refresh request");
    let outcome = bounded(manager.prompt(KEY_A, 2, "second"))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply("echo: second".into()));
    let flags = h.append_flags();
    assert_eq!(flags.len(), 2);
    assert_eq!(flags[0], flags[1]);
    assert!(!refresh.exists());
    manager.shutdown_all().await;

    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    let refresh = notice::refresh_file(&h.config.home);
    std::fs::write(&refresh, "").expect("refresh request");
    bounded(manager.prompt(KEY_A, 1, "first")).await;
    assert_eq!(h.append_flags().len(), 1);
    assert!(!refresh.exists());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn refresh_waits_for_the_running_turn_and_its_steer() {
    let h = Harness::new();
    let manager = h.steer_memory_manager("separate");
    let refresh = notice::refresh_file(&h.config.home);
    let main = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;
    std::fs::write(&refresh, "").expect("refresh request");
    let s = h.admit("refresh-s", "m2");
    assert!(manager.try_steer(KEY_STEER, s, "m2", |_| true));
    let report = bounded(main).await.expect("prompt task");
    assert_eq!(codes(&report), (None, Some((s, None))));
    assert_eq!(h.append_flags().len(), 1);
    assert!(refresh.exists(), "no restart inside the running turn");

    let next = spawn_prompt(&manager, KEY_STEER, 3, "m3");
    turn_running(&manager, KEY_STEER).await;
    assert_eq!(h.append_flags().len(), 2);
    assert!(!refresh.exists());
    let lines = h.log_lines();
    let second_start = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("START pid="))
        .nth(1)
        .map(|(at, _)| at)
        .expect("second START");
    let steer_frame = lines
        .iter()
        .position(|l| {
            serde_json::from_str::<Value>(l)
                .is_ok_and(|f| f["type"] == "prompt" && f["streamingBehavior"] == "steer")
        })
        .expect("steer prompt frame");
    assert!(steer_frame < second_start);
    manager.shutdown_all().await;
    let _ = bounded(next).await;
}

#[tokio::test]
async fn expired_file_counts_as_a_change_on_compaction() {
    const EXPIRES: &str = "2030-01-01T00:00:00Z";
    let h = Harness::new();
    let home = h.memory();
    let trip = h.memory_file("system/trip.md");
    std::fs::write(
        &trip,
        format!("---\ndescription: d\nexpires: {EXPIRES}\n---\ntrip-marker\n"),
    )
    .expect("write trip");
    let at = memory::guard::expires_at(EXPIRES).expect("instant");
    let (now, clock) = stepped_clock(at - 10);
    let manager = h
        .manager("ok")
        .with_extra_env([COMPACT])
        .with_memory(home)
        .with_clock(clock);
    bounded(manager.prompt(KEY_A, 1, "compact-now")).await;
    now.store(at + 10, Ordering::SeqCst);
    bounded(manager.prompt(KEY_A, 2, "hi")).await;
    let flags = h.append_flags();
    assert_eq!(flags.len(), 2);
    assert!(flags[0][0].contains("trip-marker"));
    assert!(!flags[1][0].contains("trip-marker"));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn no_memory_never_restarts() {
    let h = Harness::new();
    let manager = h.manager("ok").with_extra_env([COMPACT]);
    let refresh = notice::refresh_file(&h.config.home);
    std::fs::create_dir_all(refresh.parent().expect("parent")).expect("memory folder");
    std::fs::write(&refresh, "").expect("refresh request");
    for (row, text) in [(1, "compact-now"), (2, "hi"), (3, "again")] {
        bounded(manager.prompt(KEY_A, row, text)).await;
    }
    assert_eq!(h.append_flags().len(), 1);
    assert!(refresh.exists());
    manager.shutdown_all().await;
}

// --- memory notices in the owner envelope -----------------------------------------------

impl Harness {
    /// Admits and claims an owner row of `key`; returns its id.
    fn owner_row(&self, key: &str, message_id: &str) -> i64 {
        let mut store = self.store.lock().unwrap();
        let id = match store
            .admit("discord", message_id, key, "222", "111", "body")
            .expect("admit")
        {
            moraebot::store::Admitted::New(id) => id,
            moraebot::store::Admitted::Duplicate => panic!("duplicate message id"),
        };
        assert!(store.claim(id).expect("claim"));
        id
    }

    fn owner_turns(&self, row: i64) -> i64 {
        self.store
            .lock()
            .unwrap()
            .owner_turns_since_save(row)
            .expect("owner turns")
    }

    /// The `message` of every prompt frame, in order.
    fn prompt_messages(&self) -> Vec<String> {
        self.frames("prompt")
            .iter()
            .map(|f| f["message"].as_str().expect("prompt message").to_owned())
            .collect()
    }
}

/// Sends a tagged owner input for a fresh owner row of `key` and returns the row.
async fn owner_prompt(h: &Harness, manager: &OmpManager, key: &str, id: &str, body: &str) -> i64 {
    let row = h.owner_row(key, id);
    bounded(manager.prompt(key, row, &lead::owner_input(key, row, body))).await;
    row
}

/// The lines between the notice markers; empty when the message has no block.
fn notice_lines(message: &str) -> Vec<&str> {
    let lines: Vec<&str> = message.split('\n').collect();
    let Some(begin) = lines.iter().position(|l| *l == memory::NOTICE_BEGIN) else {
        return Vec::new();
    };
    let end = lines
        .iter()
        .position(|l| *l == memory::NOTICE_END)
        .expect("notice end marker");
    lines[begin + 1..end].to_vec()
}

#[tokio::test]
async fn notice_block_follows_the_tag_line() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    owner_prompt(&h, &manager, KEY_A, "nb-1", "one").await;
    h.change_persona("v2-marker");
    let commit = h.memory_commits().last().expect("commit").id;
    let r2 = owner_prompt(&h, &manager, KEY_A, "nb-2", "two").await;
    owner_prompt(&h, &manager, KEY_A, "nb-3", "three").await;

    let messages = h.prompt_messages();
    assert_eq!(messages.len(), 3);
    let soul = format!("soul path=\"system/persona.md\" writer=human commit={commit}");
    let changed =
        format!("changed kind=updated path=\"system/persona.md\" writer=human commit={commit}");
    let tag = format!("[moraebot conversation={KEY_A} message={r2}]");
    let expected = [
        tag.as_str(),
        memory::NOTICE_BEGIN,
        soul.as_str(),
        changed.as_str(),
        memory::NOTICE_END,
        "two",
    ];
    assert_eq!(messages[1].split('\n').collect::<Vec<_>>(), expected);
    assert!(!messages[0].contains(memory::NOTICE_BEGIN));
    assert!(!messages[2].contains(memory::NOTICE_BEGIN));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn lead_writes_are_not_announced() {
    let h = Harness::new();
    let manager = h.memory_tools_manager(&[create_call("notes/a.md")]);
    owner_prompt(&h, &manager, KEY_A, "lw-1", "run-tools").await;
    owner_prompt(&h, &manager, KEY_A, "lw-2", "hi").await;
    let commits = h.memory_commits();
    assert_eq!(
        commits.last().expect("commit").commit.writer,
        memory::WRITER_LEAD
    );
    let messages = h.prompt_messages();
    assert_eq!(messages.len(), 2);
    assert!(
        !messages[1].contains(memory::NOTICE_BEGIN),
        "{}",
        messages[1]
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn changed_lines_cap_at_twenty() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    owner_prompt(&h, &manager, KEY_A, "cap-1", "one").await;
    for k in 0..23 {
        let path = h.memory_file(&format!("notes/n{k}.md"));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("notes folder");
        std::fs::write(path, "---\ndescription: d\n---\nbody\n").expect("write note");
    }
    history::accept(&mut h.store.lock().unwrap(), &h.memory(), &[], "test").expect("accept");
    owner_prompt(&h, &manager, KEY_A, "cap-2", "two").await;

    let messages = h.prompt_messages();
    let lines = notice_lines(&messages[1]);
    let added = lines
        .iter()
        .filter(|l| l.starts_with("changed kind=added "))
        .count();
    assert_eq!(added, 20, "{lines:?}");
    assert!(lines.contains(&"changed more=3"), "{lines:?}");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn expired_notice_once() {
    const EXPIRES: &str = "2030-01-01T00:00:00Z";
    let h = Harness::new();
    let home = h.memory();
    let note = h.memory_file("notes/x.md");
    std::fs::create_dir_all(note.parent().expect("parent")).expect("notes folder");
    std::fs::write(
        &note,
        format!("---\ndescription: d\nexpires: {EXPIRES}\n---\nx\n"),
    )
    .expect("write note");
    let at = memory::guard::expires_at(EXPIRES).expect("instant");
    let (now, clock) = stepped_clock(at - 10);
    let manager = h.manager("ok").with_memory(home).with_clock(clock);
    owner_prompt(&h, &manager, KEY_A, "ex-1", "one").await;
    now.store(at + 10, Ordering::SeqCst);
    owner_prompt(&h, &manager, KEY_A, "ex-2", "two").await;
    owner_prompt(&h, &manager, KEY_A, "ex-3", "three").await;

    let messages = h.prompt_messages();
    assert!(notice_lines(&messages[0]).is_empty());
    assert_eq!(notice_lines(&messages[1]), ["expired path=\"notes/x.md\""]);
    assert!(notice_lines(&messages[2]).is_empty());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn compaction_notice_counts() {
    let h = Harness::new();
    let manager = h
        .manager("ok")
        .with_extra_env([COMPACT])
        .with_memory(h.memory());
    owner_prompt(&h, &manager, KEY_A, "cn-1", "compact-now").await;
    owner_prompt(&h, &manager, KEY_A, "cn-2", "two").await;
    owner_prompt(&h, &manager, KEY_A, "cn-3", "three").await;

    let messages = h.prompt_messages();
    assert!(notice_lines(&messages[0]).is_empty());
    assert_eq!(notice_lines(&messages[1]), ["compaction count=1"]);
    assert!(notice_lines(&messages[2]).is_empty());
    assert_eq!(h.append_flags().len(), 1);
    manager.shutdown_all().await;
}

fn nudge_lines(message: &str) -> Vec<&str> {
    notice_lines(message)
        .into_iter()
        .filter(|l| l.starts_with("nudge "))
        .collect()
}

#[tokio::test]
async fn nudge_after_ten_owner_turns() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    let mut nudged = false;
    for n in 0..12 {
        let row = h.owner_row(KEY_A, &format!("nu-{n}"));
        let count = h.owner_turns(row);
        bounded(manager.prompt(KEY_A, row, &lead::owner_input(KEY_A, row, "hi"))).await;
        let messages = h.prompt_messages();
        let last = messages.last().expect("prompt frame");
        if count == 10 {
            assert_eq!(nudge_lines(last), ["nudge owner_turns=10"]);
            nudged = true;
            break;
        }
        assert!(nudge_lines(last).is_empty(), "count {count}: {last}");
    }
    assert!(nudged, "the count reached 10 within 12 prompts");
    manager.shutdown_all().await;

    let h = Harness::new();
    let manager = h.memory_tools_manager(&[create_call("notes/a.md")]);
    let mut last_row = 0;
    for n in 1..=10 {
        let body = if n == 5 { "run-tools" } else { "hi" };
        last_row = owner_prompt(&h, &manager, KEY_A, &format!("nt-{n}"), body).await;
    }
    assert_eq!(h.owner_turns(last_row), 5);
    let messages = h.prompt_messages();
    assert_eq!(messages.len(), 10);
    assert!(nudge_lines(&messages[9]).is_empty(), "{}", messages[9]);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn notices_skip_lane_results() {
    let h = Harness::new();
    let manager = h.manager("ok").with_memory(h.memory());
    owner_prompt(&h, &manager, KEY_A, "ln-1", "one").await;
    h.change_persona("v2-marker");
    let lane_input = lead::lane_result(KEY_A, 1, &lead::LaneResult::Text("t"));
    bounded(manager.prompt(KEY_A, 900, &lane_input)).await;
    owner_prompt(&h, &manager, KEY_A, "ln-3", "three").await;

    let messages = h.prompt_messages();
    assert_eq!(messages[1], lane_input);
    assert!(
        notice_lines(&messages[2])
            .iter()
            .any(|l| l.starts_with("changed kind=updated path=\"system/persona.md\" ")),
        "{}",
        messages[2]
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn steer_carries_notices_without_nudge() {
    let h = Harness::new();
    let manager = h.steer_memory_manager("separate");
    let mut main_row = 0;
    for n in 0..12 {
        main_row = h.owner_row(KEY_STEER, &format!("st-{n}"));
        if h.owner_turns(main_row) == 10 {
            break;
        }
    }
    assert_eq!(h.owner_turns(main_row), 10);
    let main_text = lead::owner_input(KEY_STEER, main_row, "m1");
    let main = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.prompt(KEY_STEER, main_row, &main_text).await })
    };
    turn_running(&manager, KEY_STEER).await;
    h.change_persona("v2-marker");
    let s = h.admit("st-steer", "m2");
    let steer_text = lead::owner_input(KEY_STEER, s, "m2");
    assert!(manager.try_steer(KEY_STEER, s, &steer_text, |main| {
        h.store.lock().unwrap().steer_into(s, main).unwrap()
    }));
    assert_eq!(h.owner_turns(s), 10);
    let report = bounded(main).await.expect("prompt task");
    assert_eq!(codes(&report), (None, Some((s, None))));

    let frames = h.frames("prompt");
    let main_frame = frames
        .iter()
        .find(|f| f["streamingBehavior"] == "followUp")
        .expect("main frame");
    let steer_frame = h.steer_frames().pop().expect("steer frame");
    assert_eq!(
        notice_lines(main_frame["message"].as_str().unwrap()),
        ["nudge owner_turns=10"]
    );
    let steer_lines = notice_lines(steer_frame["message"].as_str().unwrap());
    assert!(
        steer_lines
            .iter()
            .any(|l| l.starts_with("changed kind=updated path=\"system/persona.md\" ")),
        "{steer_lines:?}"
    );
    assert!(nudge_lines(steer_frame["message"].as_str().unwrap()).is_empty());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn restart_moves_the_marks() {
    let h = Harness::new();
    let manager = h
        .manager("ok")
        .with_extra_env([COMPACT])
        .with_memory(h.memory());
    owner_prompt(&h, &manager, KEY_A, "rm-1", "compact-now").await;
    h.change_persona("v2-marker");
    owner_prompt(&h, &manager, KEY_A, "rm-2", "two").await;

    assert_eq!(h.append_flags().len(), 2);
    let messages = h.prompt_messages();
    assert_eq!(notice_lines(&messages[1]), ["compaction count=1"]);
    manager.shutdown_all().await;
}

fn record_call() -> Value {
    tool_call(
        memory::MEMORY_AGREEMENT_RECORD_TOOL,
        serde_json::json!({"scope": "global", "text": "never push to main", "reason": "r"}),
    )
}

fn break_call() -> Value {
    tool_call(
        memory::MEMORY_AGREEMENT_BREAK_TOOL,
        serde_json::json!({"id": 1, "reason": "r"}),
    )
}

/// The JSON of a tool result that is not an error.
fn ok_json(result: &(bool, String)) -> Value {
    assert!(!result.0, "{result:?}");
    serde_json::from_str(&result.1).expect("tool result json")
}

fn agreement_lines(message: &str) -> Vec<&str> {
    notice_lines(message)
        .into_iter()
        .filter(|l| l.starts_with("agreement "))
        .collect()
}

#[tokio::test]
async fn agreement_record_and_break_through_the_lead() {
    let h = Harness::new();
    let manager = h.memory_tools_manager(&[record_call(), break_call()]);
    for id in ["a1", "a2", "a3"] {
        owner_prompt(&h, &manager, KEY_A, id, "run-tools").await;
    }
    owner_prompt(&h, &manager, KEY_A, "a4", "hi").await;
    owner_prompt(&h, &manager, KEY_A, "a5", "again").await;

    let results = h.tool_results();
    assert_eq!(results.len(), 6, "{results:?}");
    let recorded = ok_json(&results[0]);
    assert_eq!(recorded["agreement"], 1, "{recorded}");
    assert_eq!(recorded["author"], "agent", "{recorded}");
    assert_eq!(ok_json(&results[1])["broken"], 1);
    assert!(
        refused_with(&results[2], memory::E_MEMORY_AGREEMENT_EXISTS),
        "{results:?}"
    );
    assert_eq!(ok_json(&results[3])["broken"], 2);
    assert!(
        refused_with(&results[4], memory::E_MEMORY_AGREEMENT_EXISTS),
        "{results:?}"
    );
    assert_eq!(
        ok_json(&results[5]),
        serde_json::json!({"agreement": 1, "broken": 3, "needs_human": true, "flagged": true})
    );

    let messages = h.prompt_messages();
    assert_eq!(messages.len(), 5);
    assert_eq!(
        agreement_lines(&messages[3]),
        ["agreement needs-human id=1 broken=3 text=\"never push to main\""]
    );
    for n in [0, 1, 2, 4] {
        assert!(agreement_lines(&messages[n]).is_empty(), "{}", messages[n]);
    }
    assert_eq!(h.append_flags().len(), 1, "exactly one START line");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn owner_quoted_agreement_is_human_through_the_lead() {
    let h = Harness::new();
    let (_, r) = proposal_then_answer(&h, "yes");
    let call = tool_call(
        memory::MEMORY_AGREEMENT_RECORD_TOOL,
        serde_json::json!({
            "scope": "global",
            "text": "never push to main",
            "reason": "r",
            "source": "user",
            "user_quote": "yes",
            "quote_message": r,
        }),
    );
    let manager = h.memory_tools_manager(&[call]);
    bounded(manager.prompt(KEY_A, r, "run-tools")).await;

    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(ok_json(&results[0])["author"], "human");
    let rows = h.store.lock().unwrap().agreements().expect("agreements");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].author, "human");
    manager.shutdown_all().await;
}

fn boundaries_create(claim: Value) -> Value {
    let mut arguments = serde_json::json!({
        "path": memory::BOUNDARIES,
        "description": "don't-rules",
        "body": "- never force push\n",
        "reason": "r",
    });
    for (key, value) in claim.as_object().expect("claim object") {
        arguments[key] = value.clone();
    }
    tool_call(memory::MEMORY_CREATE_TOOL, arguments)
}

#[tokio::test]
async fn boundaries_write_needs_the_quote_through_the_lead() {
    let h = Harness::new();
    let manager =
        h.memory_tools_manager(&[boundaries_create(serde_json::json!({"source": "agent"}))]);
    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        refused_with(&results[0], memory::E_MEMORY_QUOTE),
        "{results:?}"
    );
    assert_eq!(h.memory_commits().len(), 1, "the import only");
    assert!(!h.memory_file(memory::BOUNDARIES).exists());
    manager.shutdown_all().await;

    let h = Harness::new();
    let (_, r) = proposal_then_answer(&h, "yes");
    let approved = boundaries_create(
        serde_json::json!({"source": "user", "user_quote": "yes", "quote_message": r}),
    );
    let manager = h.memory_tools_manager(&[approved]);
    bounded(manager.prompt(KEY_A, r, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(ok_json(&results[0])["soul_edit"], true);
    assert!(h.memory_file(memory::BOUNDARIES).exists());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn agreements_file_is_refused_through_the_lead() {
    let h = Harness::new();
    let manager = h.memory_tools_manager(&[create_call(memory::AGREEMENTS_FILE)]);
    bounded(manager.prompt(KEY_A, 1, "run-tools")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        refused_with(&results[0], memory::E_MEMORY_AGREEMENT),
        "{results:?}"
    );
    assert!(!h.memory_file(memory::AGREEMENTS_FILE).exists());
    manager.shutdown_all().await;
}

#[tokio::test]
async fn agreements_show_after_refresh() {
    let h = Harness::new();
    let manager = h.memory_tools_manager(&[record_call()]);
    owner_prompt(&h, &manager, KEY_A, "b1", "run-tools").await;
    std::fs::write(notice::refresh_file(&h.config.home), "").expect("refresh request");
    owner_prompt(&h, &manager, KEY_A, "b2", "hi").await;

    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(ok_json(&results[0])["agreement"], 1);
    let flags = h.append_flags();
    assert_eq!(flags.len(), 2, "exactly two START lines");
    assert!(!flags[0][0].contains("<agreements>\n"));
    assert!(flags[1][0].contains("<agreements>\n"));
    assert!(
        flags[1][0]
            .lines()
            .any(|line| line == "- #1 [global] [agent] never push to main")
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn no_memory_means_no_notice() {
    let h = Harness::new();
    let manager = h.manager("ok").with_extra_env([COMPACT]);
    for (n, body) in ["compact-now", "two", "three"].into_iter().enumerate() {
        owner_prompt(&h, &manager, KEY_A, &format!("nm-{n}"), body).await;
    }
    let messages = h.prompt_messages();
    assert_eq!(messages.len(), 3);
    assert!(messages.iter().all(|m| !m.contains(memory::NOTICE_BEGIN)));
    manager.shutdown_all().await;
}

// --- onboarding in the lead --------------------------------------------------------------

/// Starts the onboarding; the harness seeds rows 1 and 2, so it counts from row 2.
fn start_onboarding(h: &Harness) {
    let after = h
        .store
        .lock()
        .unwrap()
        .start_onboarding()
        .expect("start onboarding");
    assert_eq!(after, 2);
}

fn onboarding_lines(message: &str) -> Vec<&str> {
    notice_lines(message)
        .into_iter()
        .filter(|l| l.starts_with("onboarding "))
        .collect()
}

fn onboarding_row(h: &Harness) -> Option<OnboardingRow> {
    h.store
        .lock()
        .unwrap()
        .onboarding()
        .expect("onboarding row")
}

#[tokio::test]
async fn onboarding_turns_reach_the_lead() {
    let h = Harness::new();
    start_onboarding(&h);
    let manager = h.manager("ok").with_memory(h.memory());
    owner_prompt(&h, &manager, KEY_A, "m-on-1", "hi").await;
    owner_prompt(&h, &manager, KEY_A, "m-on-2", "again").await;
    let messages = h.prompt_messages();
    assert_eq!(messages.len(), 2);
    assert!(
        notice_lines(&messages[0]).contains(&"onboarding turn=1"),
        "{}",
        messages[0]
    );
    assert!(
        notice_lines(&messages[1]).contains(&"onboarding turn=2"),
        "{}",
        messages[1]
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn onboarding_line_stops_after_the_limit() {
    let h = Harness::new();
    start_onboarding(&h);
    let manager = h.manager("ok").with_memory(h.memory());
    for n in 0..11 {
        owner_prompt(&h, &manager, KEY_A, &format!("m-lim-{n}"), "hi").await;
    }
    let messages = h.prompt_messages();
    assert_eq!(messages.len(), 11);
    for (n, message) in messages[..10].iter().enumerate() {
        let expected = format!("onboarding turn={}", n + 1);
        assert_eq!(onboarding_lines(message), [expected.as_str()], "{message}");
    }
    assert!(
        onboarding_lines(&messages[10]).is_empty(),
        "{}",
        messages[10]
    );
    assert_eq!(
        onboarding_row(&h).map(|row| row.state),
        Some(OnboardingState::Active)
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn onboarding_finish_through_the_lead() {
    const BODY: &str = "Call yourself Sora. run-tools";
    let h = Harness::new();
    start_onboarding(&h);
    let r = {
        let mut store = h.store.lock().unwrap();
        let r = match store
            .admit("discord", "m-sora", KEY_A, "222", "111", BODY)
            .expect("admit")
        {
            moraebot::store::Admitted::New(id) => id,
            other => panic!("expected a new row, got {other:?}"),
        };
        assert!(store.claim(r).expect("claim"));
        r
    };
    let calls = [
        tool_call(
            memory::MEMORY_CREATE_TOOL,
            serde_json::json!({
                "path": memory::IDENTITY,
                "description": "What the lead is called",
                "body": "Name: Sora\n",
                "reason": "r",
                "source": "user",
                "user_quote": "Call yourself Sora",
                "quote_message": r,
            }),
        ),
        tool_call(
            memory::ONBOARDING_FINISH_TOOL,
            serde_json::json!({"outcome": "done", "reason": "r"}),
        ),
    ];
    let manager = h.memory_tools_manager(&calls);
    bounded(manager.prompt(KEY_A, r, &lead::owner_input(KEY_A, r, BODY))).await;

    let results = h.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(ok_json(&results[0])["soul_edit"], true);
    assert_eq!(
        ok_json(&results[1]),
        serde_json::json!({"onboarding": "done", "refresh": true})
    );
    let provenance = file_provenance(&h, memory::IDENTITY);
    assert!(
        provenance.starts_with(&format!(
            "quoted: owner message {r} contains \"Call yourself Sora\""
        )),
        "{provenance}"
    );
    assert_eq!(
        onboarding_row(&h),
        Some(OnboardingRow {
            state: OnboardingState::Done,
            after_row: 2
        })
    );

    owner_prompt(&h, &manager, KEY_A, "m-after", "again").await;
    assert_eq!(h.start_pids().len(), 2, "the refresh re-pins the lead");
    let messages = h.prompt_messages();
    let last = messages.last().expect("last prompt");
    assert!(onboarding_lines(last).is_empty(), "{last}");
    let flags = h.append_flags();
    let last_flags = flags.last().expect("last START line");
    assert!(last_flags[0].contains("Name: Sora"), "{last_flags:?}");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn onboarding_finish_without_onboarding_is_refused() {
    let h = Harness::new();
    let call = tool_call(
        memory::ONBOARDING_FINISH_TOOL,
        serde_json::json!({"outcome": "done", "reason": "r"}),
    );
    let manager = h.memory_tools_manager(&[call]);
    owner_prompt(&h, &manager, KEY_A, "m-none", "run-tools").await;

    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(
        refused_with(&results[0], memory::E_ONBOARDING_STATE),
        "{results:?}"
    );
    assert_eq!(onboarding_row(&h), None);
    assert!(!notice::refresh_file(&h.config.home).exists());
    manager.shutdown_all().await;
}
