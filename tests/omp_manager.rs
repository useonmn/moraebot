use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
use moraebot::done;
use moraebot::files;
use moraebot::lane;
use moraebot::lead;
use moraebot::memory::home::{MemoryHome, PERSONA_SEED};
use moraebot::memory::{self, frontmatter, history};
use moraebot::omp::{self, GroupReport, OmpManager, Outcome};
use moraebot::store::{LaneStart, LaneState, Store};
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
    bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
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
    bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
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
    bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
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
    bounded(manager.prompt(KEY_A, 1, "hi")).await.main.outcome;
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
    bounded(manager.prompt(KEY_A, 1, "first"))
        .await
        .main
        .outcome;
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
            } else if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                if frame["type"] == "set_host_tools" {
                    let names = frame["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| t["name"].as_str().unwrap().to_owned())
                        .collect();
                    out.push((lane, names));
                }
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
