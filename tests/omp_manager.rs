use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::access::Sender;
use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform, Role};
use moraebot::files;
use moraebot::lead;
use moraebot::omp::{self, GroupReport, OmpManager, Outcome};
use moraebot::onboarding;
use moraebot::persona;
use moraebot::store::{Admitted, OnboardingRow, OnboardingState, Store};
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
                access: Default::default(),
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
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
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

#[tokio::test]
async fn max_cost_adds_flag_only_when_set() {
    let h = Harness::with_omp(OmpConfig {
        turn_timeout_minutes: None,
        max_cost_usd: Some(2.5),
    });
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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

/// Per `START` line in log order, its `--append-system-prompt=` argv elements.
fn appends(h: &Harness) -> Vec<Vec<String>> {
    h.log_lines()
        .iter()
        .filter(|l| l.starts_with("START pid="))
        .map(|l| {
            start_argv(l)
                .into_iter()
                .filter(|a| a.starts_with("--append-system-prompt="))
                .collect()
        })
        .collect()
}

fn save_persona(h: &Harness, name: &str, tone: &str, owner: &str) {
    let text = persona::render(name, tone, owner).unwrap();
    persona::save(&h.config.home, &text).unwrap();
}

#[tokio::test]
async fn lead_appends_persona_after_hints() {
    let h = Harness::new();
    save_persona(
        &h,
        "Sora",
        "short and warm",
        "Owner 111 likes short replies",
    );
    let manager = h.manager("ok").with_hints("H".into());
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
    let loaded = persona::load(&h.config.home).unwrap();
    let expected = format!(
        "--append-system-prompt={}",
        persona::append("H", loaded.as_deref())
    );
    let appends = appends(&h);
    assert_eq!(appends, vec![vec![expected]]);
    assert!(appends[0][0].contains("Name: Sora"));
    bounded(manager.shutdown_all()).await;
}

fn save_sora(h: &Harness) {
    save_persona(h, "Sora", "short and warm", "Owner 111 likes short replies");
}

#[tokio::test]
async fn persona_change_restarts_the_lead_before_the_next_turn() {
    let h = Harness::new();
    save_sora(&h);
    let m = h.manager("ok");
    let r1 = bounded(m.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
    save_persona(&h, "Kai", "", "");
    let r2 = bounded(m.prompt(KEY_B, 2, Sender::Owner, "again"))
        .await
        .main
        .outcome;
    let pids = h.start_pids();
    let first_alive = pids.first().map(|p| alive(*p));
    let leases = h.frames("get_session_lease");
    let rest: Vec<Vec<String>> = h
        .log_lines()
        .iter()
        .filter(|l| l.starts_with("START pid="))
        .map(|l| {
            start_argv(l)
                .into_iter()
                .filter(|a| !a.starts_with("--append-system-prompt="))
                .collect()
        })
        .collect();
    let appends = appends(&h);
    bounded(m.shutdown_all()).await;

    assert_eq!(r1, Outcome::Reply("echo: hi".into()));
    assert_eq!(r2, Outcome::Reply("echo: again".into()));
    assert_eq!(pids.len(), 2);
    assert_eq!(first_alive, Some(false));
    assert_eq!(leases.len(), 1);
    assert_eq!(
        leases[0]["sessionIds"],
        serde_json::json!(["fake-session-1"])
    );
    assert_eq!(rest.len(), 2);
    for argv in &rest {
        assert_eq!(argv, &expected_argv(&h.config));
    }
    assert_eq!(appends.len(), 2);
    let first = appends[0].concat();
    let second = appends[1].concat();
    assert!(first.contains("Name: Sora") && !first.contains("Name: Kai"));
    assert!(second.contains("Name: Kai") && !second.contains("Name: Sora"));
}

#[tokio::test]
async fn unchanged_persona_keeps_the_lead() {
    let h = Harness::new();
    save_sora(&h);
    let m = h.manager("ok");
    bounded(m.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
    save_sora(&h);
    bounded(m.prompt(KEY_B, 2, Sender::Owner, "again")).await;
    bounded(m.prompt(KEY_A, 3, Sender::Owner, "more")).await;
    let pids = h.start_pids();
    bounded(m.shutdown_all()).await;

    assert_eq!(pids.len(), 1);
}

#[tokio::test]
async fn removed_persona_restarts_without_it() {
    let h = Harness::new();
    save_sora(&h);
    let first = format!(
        "--append-system-prompt={}",
        persona::append("H", persona::load(&h.config.home).unwrap().as_deref())
    );
    let m = h.manager("ok").with_hints("H".into());
    bounded(m.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
    std::fs::remove_file(persona::path(&h.config.home)).unwrap();
    bounded(m.prompt(KEY_B, 2, Sender::Owner, "again")).await;
    let appends = appends(&h);
    bounded(m.shutdown_all()).await;

    assert_eq!(
        appends,
        vec![vec![first], vec!["--append-system-prompt=H".to_owned()]]
    );
}

#[tokio::test]
async fn refused_persona_change_restarts_without_it() {
    let h = Harness::new();
    save_sora(&h);
    let m = h.manager("ok").with_hints("H".into());
    bounded(m.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
    let secret = ["gl", "pat-"].concat() + &"c".repeat(20);
    std::fs::write(
        persona::path(&h.config.home),
        format!("Name: X\nOwner: {secret}"),
    )
    .unwrap();
    bounded(m.prompt(KEY_B, 2, Sender::Owner, "again")).await;
    bounded(m.prompt(KEY_A, 3, Sender::Owner, "more")).await;
    let appends = appends(&h);
    let code = persona::load(&h.config.home).unwrap_err().code;
    bounded(m.shutdown_all()).await;

    assert_eq!(appends.len(), 2);
    assert!(appends[0][0].contains("Name: Sora"));
    assert_eq!(appends[1], vec!["--append-system-prompt=H".to_owned()]);
    assert_eq!(code, persona::E_PERSONA_SECRET);
}

#[tokio::test]
async fn onboarding_finish_applies_from_the_next_turn() {
    let h = Harness::new();
    h.store.lock().unwrap().start_onboarding().unwrap();
    let r1 = owner_row(&h, "m-on-1", "run-tools");
    let m = onboarding_manager(&h, &[finish_done_call()]);
    bounded(m.prompt(
        KEY_A,
        r1,
        Sender::Owner,
        &lead::owner_input(KEY_A, r1, "run-tools"),
    ))
    .await;
    let after_first = h.start_pids().len();
    let r2 = owner_row(&h, "m-on-2", "again");
    bounded(m.prompt(
        KEY_A,
        r2,
        Sender::Owner,
        &lead::owner_input(KEY_A, r2, "again"),
    ))
    .await;
    let pids = h.start_pids();
    let appends = appends(&h);
    let tools = h.frames("set_host_tools");
    bounded(m.shutdown_all()).await;

    assert_eq!(after_first, 1);
    assert_eq!(pids.len(), 2);
    assert!(!appends[0].concat().contains("Name: Sora"));
    assert!(appends[1].concat().contains("Name: Sora"));
    assert_eq!(tools.len(), 2);
    for frame in &tools {
        let names: Vec<&str> = frame["tools"]
            .as_array()
            .expect("tool list")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&onboarding::FINISH_TOOL), "{names:?}");
    }
}

#[tokio::test]
async fn unsettled_session_defers_the_restart_until_settled() {
    let mut h = Harness::new();
    h.config.omp_path = unsettled_once_wrapper(h.dir.path());
    save_sora(&h);
    let m = h.manager("ok");
    let r1 = bounded(m.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
    save_persona(&h, "Kai", "", "");
    let r2 = bounded(m.prompt(KEY_B, 2, Sender::Owner, "again"))
        .await
        .main
        .outcome;
    let after_second = h.start_pids().len();
    let r3 = bounded(m.prompt(KEY_A, 3, Sender::Owner, "more"))
        .await
        .main
        .outcome;
    let pids = h.start_pids();
    let appends = appends(&h);
    bounded(m.shutdown_all()).await;

    assert_eq!(r1, Outcome::Reply("echo: hi".into()));
    assert_eq!(r2, Outcome::Reply("echo: again".into()));
    assert_eq!(r3, Outcome::Reply("echo: more".into()));
    assert_eq!(after_second, 1);
    assert_eq!(pids.len(), 2);
    assert_eq!(appends.len(), 2);
    let first = appends[0].concat();
    let second = appends[1].concat();
    assert!(first.contains("Name: Sora") && !first.contains("Name: Kai"));
    assert!(second.contains("Name: Kai") && !second.contains("Name: Sora"));
}

#[tokio::test]
async fn failed_restart_reports_the_start_failure_then_starts_again() {
    let mut h = Harness::new();
    let marker = h.dir.path().join("refuse-start");
    h.config.omp_path = refusing_wrapper(h.dir.path(), &marker);
    save_sora(&h);
    let m = h.manager("ok");
    let r1 = bounded(m.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
    std::fs::write(&marker, "").unwrap();
    save_persona(&h, "Kai", "", "");
    let r2 = bounded(m.prompt(KEY_B, 2, Sender::Owner, "again"))
        .await
        .main
        .outcome;
    let after_fail = h.start_pids();
    let first_alive = after_fail.first().map(|p| alive(*p));
    let child_after_fail = m.child_pid().await;
    std::fs::remove_file(&marker).unwrap();
    let r3 = bounded(m.prompt(KEY_A, 3, Sender::Owner, "more"))
        .await
        .main
        .outcome;
    let pids = h.start_pids();
    let appends = appends(&h);
    bounded(m.shutdown_all()).await;

    assert_eq!(r1, Outcome::Reply("echo: hi".into()));
    assert!(matches!(r2, Outcome::Failed { .. }), "{r2:?}");
    assert_eq!(r2.code(), Some(omp::E_OMP_SPAWN));
    assert_eq!(after_fail.len(), 1);
    assert_eq!(first_alive, Some(false));
    assert_eq!(child_after_fail, None);
    assert_eq!(r3, Outcome::Reply("echo: more".into()));
    assert_eq!(pids.len(), 2);
    assert_eq!(appends.len(), 2);
    assert!(appends[1].concat().contains("Name: Kai"));
}

#[tokio::test]
async fn refused_persona_is_left_out() {
    let h = Harness::new();
    let secret = ["gl", "pat-"].concat() + &"c".repeat(20);
    std::fs::write(
        persona::path(&h.config.home),
        format!("Name: X\nOwner: {secret}"),
    )
    .unwrap();
    let manager = h.manager("ok").with_hints("H".into());
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
    assert_eq!(
        appends(&h),
        vec![vec!["--append-system-prompt=H".to_owned()]]
    );
    assert_eq!(
        persona::load(&h.config.home).unwrap_err().code,
        persona::E_PERSONA_SECRET
    );
    bounded(manager.shutdown_all()).await;
}

#[tokio::test]
async fn injection_payload_arrives_byte_for_byte_with_hints_set() {
    let h = Harness::new();
    let manager = h.manager("ok").with_hints("probe hints".into());
    let payload = "$(touch /tmp/moraebot-pwned-7f3a); touch /tmp/moraebot-pwned-7f3a-b";
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, payload))
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
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, text))
        .await
        .main
        .outcome;
    assert_eq!(outcome, Outcome::Reply(format!("echo: {text}")));
    manager.shutdown_all().await;
}

#[tokio::test]
async fn injection_payload_arrives_byte_for_byte_and_never_runs() {
    let h = Harness::new();
    let manager = h.manager("ok");
    let payload = "$(touch /tmp/moraebot-pwned-7f3a); touch /tmp/moraebot-pwned-7f3a-b";
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, payload))
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
        bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
            .await
            .main
            .outcome,
        Outcome::NoAgent
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn error_status_is_failed() {
    let h = Harness::new();
    let manager = h.manager("error");
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
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

/// Each child's first `prompt_result` reports `sessionSettled: false`; later ones are unchanged.
fn unsettled_once_wrapper(root: &Path) -> PathBuf {
    let script = root.join("omp-unsettled-once.sh");
    let fake = env!("CARGO_BIN_EXE_fake-omp");
    let body = format!(
        r#"#!/bin/sh
'{fake}' "$@" | awk '{{ if (!n && sub(/"sessionSettled":true/, "\"sessionSettled\":false")) n = 1; print; fflush() }}'
"#
    );
    std::fs::write(&script, body).expect("write wrapper");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    script
}

/// Exits before omp's `ready` frame while `marker` exists, so the child start fails.
fn refusing_wrapper(root: &Path, marker: &Path) -> PathBuf {
    let script = root.join("omp-refusing.sh");
    let fake = env!("CARGO_BIN_EXE_fake-omp");
    let marker = marker.display();
    let body = format!(
        r#"#!/bin/sh
if [ -e '{marker}' ]; then exit 3; fi
exec '{fake}' "$@"
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
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
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
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
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
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
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
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
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
        bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
            .await
            .main
            .outcome,
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
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
    assert!(h.frames("get_session_lease").is_empty());
    assert_eq!(h.frames("open_session").len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn one_lead_child_serves_every_conversation() {
    let h = Harness::new();
    let manager = h.manager("ok");

    let first = bounded(manager.prompt("discord:th:111", 1, Sender::Owner, "one")).await;
    assert_eq!(first.main.outcome, Outcome::Reply("echo: one".into()));
    let pid = manager.child_pid().await.expect("live lead child");
    let second = bounded(manager.prompt("discord:dm:222", 2, Sender::Owner, "two")).await;
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
    bounded(first.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
        bounded(second.prompt(KEY_B, 2, Sender::Owner, "again"))
            .await
            .main
            .outcome,
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
    let outcome = bounded(third.prompt(KEY_A, 3, Sender::Owner, "busy"))
        .await
        .main
        .outcome;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }), "{outcome:?}");
    assert_eq!(outcome.code(), Some(omp::E_SESSION_BUSY));
}

#[tokio::test]
async fn killed_child_is_lost_then_respawned() {
    let h = Harness::new();
    let manager = h.manager("ok");
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "first")).await;
    let pid = manager.child_pid().await.expect("live child");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success());

    let outcome = bounded(manager.prompt(KEY_A, 2, Sender::Owner, "second"))
        .await
        .main
        .outcome;
    assert!(matches!(outcome, Outcome::NeedsCheck { .. }));
    assert_eq!(outcome.code(), Some(omp::E_CHILD_LOST));
    assert_eq!(manager.child_pid().await, None);
    assert!(!alive(pid));

    let outcome = bounded(manager.prompt(KEY_A, 3, Sender::Owner, "third"))
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
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
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
    let outcome = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi"))
        .await
        .main
        .outcome;
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
    let report = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    let report = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    let report = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    let report = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    let report = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    let first = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "one")).await;
    assert_eq!(first.main.files, [a.clone(), b.clone()]);
    let second = bounded(manager.prompt(KEY_A, 2, Sender::Owner, "two")).await;
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
    let report = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    let report = bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    tokio::spawn(async move { manager.prompt(key, row_id, Sender::Owner, text).await })
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
    bounded(manager.prompt(KEY_A, 1, Sender::Owner, "hi")).await;
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
    assert!(manager.try_steer(KEY_STEER, 2, Sender::Owner, "m2", |_| true));
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
    assert!(manager.try_steer(KEY_STEER, 2, Sender::Owner, "m2", |_| true));
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
    assert!(
        !manager.try_steer("discord:th:999", 2, Sender::Owner, "other", |_| {
            called = true;
            true
        })
    );
    assert!(!called, "another conversation never reaches the claim");

    assert!(manager.try_steer(KEY_STEER, 3, Sender::Owner, "m2", |_| true));
    let report = bounded(turn).await.expect("prompt task");
    assert_eq!(report.steered.map(|(id, _)| id), Some(3));

    assert!(
        !manager.try_steer(KEY_STEER, 4, Sender::Owner, "late", |_| {
            called = true;
            true
        })
    );
    assert!(!called, "a closed turn never reaches the claim");
    assert_eq!(*manager.running_turn().borrow(), None);
    assert_eq!(h.steer_frames().len(), 1);
    manager.shutdown_all().await;
}

#[tokio::test]
async fn steer_refused_from_another_sender() {
    let h = Harness::new();
    let manager = h.steer_manager("merged");
    let turn = spawn_prompt(&manager, KEY_STEER, 1, "m1");
    turn_running(&manager, KEY_STEER).await;

    let mut called = false;
    let chat = Sender::Listed(Role::Chat);
    assert!(!manager.try_steer(KEY_STEER, 2, chat, "chat", |_| {
        called = true;
        true
    }));
    assert!(!called, "a non-owner message never joins the owner's turn");

    assert!(manager.try_steer(KEY_STEER, 3, Sender::Owner, "m2", |_| true));
    let report = bounded(turn).await.expect("prompt task");
    assert_eq!(report.steered.map(|(id, _)| id), Some(3));
    let steers = h.steer_frames();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["message"], "m2");
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

    let report = bounded(manager.prompt(KEY_STEER, 7, Sender::Owner, "run-tools ctx")).await;
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

/// A `tools` lead that makes `calls` (one JSON line each) and has the onboarding tool.
fn onboarding_manager(h: &Harness, calls: &[Value]) -> OmpManager {
    let lines: Vec<String> = calls.iter().map(Value::to_string).collect();
    let m = h
        .manager("tools")
        .with_extra_env([("FAKE_OMP_TOOL_CALLS", lines.join("\n"))]);
    let tools = onboarding::host_tools(
        Arc::clone(&h.store),
        h.config.home.clone(),
        m.turn_context(),
    );
    m.with_host_tools(tools)
}

/// Admits a claimed owner row on `KEY_A` and returns its id.
fn owner_row(h: &Harness, message_id: &str, text: &str) -> i64 {
    let mut store = h.store.lock().unwrap();
    let Admitted::New(id) = store
        .admit("discord", message_id, KEY_A, "222", "111", text)
        .expect("admit owner row")
    else {
        panic!("owner row is new");
    };
    assert!(store.claim(id).unwrap());
    id
}

fn finish_done_call() -> Value {
    serde_json::json!({
        "tool": onboarding::FINISH_TOOL,
        "arguments": {
            "outcome": "done",
            "name": "Sora",
            "tone": "short and warm",
            "owner": "Owner 111 likes short replies"
        }
    })
}

#[tokio::test]
async fn onboarding_finish_through_the_lead() {
    let h = Harness::new();
    assert_eq!(h.store.lock().unwrap().start_onboarding().unwrap(), 2);
    let r = owner_row(&h, "m-on-1", "run-tools");
    assert_eq!(r, 3);
    let m = onboarding_manager(&h, &[finish_done_call()]);
    bounded(m.prompt(
        KEY_A,
        r,
        Sender::Owner,
        &lead::owner_input(KEY_A, r, "run-tools"),
    ))
    .await;

    let tools = h.frames("set_host_tools");
    assert_eq!(tools.len(), 1);
    let names: Vec<&str> = tools[0]["tools"]
        .as_array()
        .expect("tool list")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&onboarding::FINISH_TOOL), "{names:?}");
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    let result: Value = serde_json::from_str(&results[0].1).expect("result json");
    assert_eq!(
        result,
        serde_json::json!({"onboarding": "done", "persona": "saved", "applies": "next_turn"})
    );
    assert_eq!(
        std::fs::read_to_string(persona::path(&h.config.home)).expect("persona file"),
        persona::render("Sora", "short and warm", "Owner 111 likes short replies").unwrap()
    );
    assert_eq!(
        h.store.lock().unwrap().onboarding().unwrap(),
        Some(OnboardingRow {
            state: OnboardingState::Done,
            after_row: 2
        })
    );
    assert_eq!(h.start_pids().len(), 1);
    m.shutdown_all().await;
}

#[tokio::test]
async fn onboarding_finish_refusal_reaches_the_lead() {
    let h = Harness::new();
    let r = owner_row(&h, "m-on-1", "run-tools");
    let m = onboarding_manager(&h, &[finish_done_call()]);
    bounded(m.prompt(
        KEY_A,
        r,
        Sender::Owner,
        &lead::owner_input(KEY_A, r, "run-tools"),
    ))
    .await;

    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].0, "{results:?}");
    assert!(
        results[0].1.starts_with(onboarding::E_ONBOARDING_STATE),
        "{results:?}"
    );
    assert!(!persona::path(&h.config.home).exists());
    m.shutdown_all().await;
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
    let steered_r = manager.try_steer(KEY_STEER, r, Sender::Owner, "mR", |main| {
        called = Some(main);
        h.store.lock().unwrap().steer_into(r, main).unwrap()
    });
    assert!(!steered_r);
    assert_eq!(called, Some(a), "the claim got the open turn's main row");

    let steered_s = manager.try_steer(KEY_STEER, s, Sender::Owner, "mS", |main| {
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
    assert!(manager.try_steer(KEY_STEER, 2, Sender::Owner, "m2", |_| true));

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
    assert!(manager.try_steer(KEY_STEER, 2, Sender::Owner, "m2", |_| true));
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
    assert!(manager.try_steer(KEY_STEER, 2, Sender::Owner, "m2", |_| true));

    let mut called = false;
    assert!(!manager.try_steer(KEY_STEER, 3, Sender::Owner, "m3", |_| {
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
    assert!(manager.try_steer(KEY_STEER, 2, Sender::Owner, "m2", |_| true));

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
