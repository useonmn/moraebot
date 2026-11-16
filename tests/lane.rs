//! Lanes driven through a real `OmpManager` with fake-omp as lead (`FAKE_OMP_MODE=tools`)
//! and as lanes (`FAKE_OMP_LANE_MODE`). Only the lead prompt that should call tools
//! contains the trigger `run-tools`, so each test starts an exact number of lanes. Every
//! test ends with `shutdown_all`, which releases held lanes.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
use moraebot::lane::{
    self, E_LANE_ARGS, E_LANE_BUSY, E_LANE_INTERRUPTED, E_LANE_LIMIT, E_LANE_STOPPING,
    E_LANE_UNKNOWN,
};
use moraebot::lead;
use moraebot::omp::{self, OmpManager, Outcome};
use moraebot::store::{LaneState, Store};
use serde_json::{Value, json};

const KEY: &str = "discord:th:111";
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

    /// A manager whose lead runs `tools` with `calls` (one JSON object per line) and
    /// whose lanes run `lane_mode`.
    fn manager(&self, lane_mode: &str, calls: &[Value]) -> OmpManager {
        let script: Vec<String> = calls.iter().map(Value::to_string).collect();
        OmpManager::new(self.config.clone(), Arc::clone(&self.store)).with_extra_env([
            ("FAKE_OMP_MODE", "tools".to_owned()),
            ("FAKE_OMP_LANE_MODE", lane_mode.to_owned()),
            ("FAKE_OMP_TOOL_CALLS", script.join("\n")),
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

    /// `(is_error, text)` of every `host_tool_result` the fake received, in order.
    fn tool_results(&self) -> Vec<(bool, String)> {
        self.log_lines()
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|f| f["type"] == "host_tool_result")
            .map(|f| {
                (
                    f["isError"] == true,
                    f["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect()
    }

    /// Indexes (in the log) of the `START` lines of lane children.
    fn lane_starts(&self) -> Vec<usize> {
        self.log_lines()
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("START pid="))
            .filter(|(_, l)| session_dir_arg(l).contains("/lanes/"))
            .map(|(n, _)| n)
            .collect()
    }

    fn lane(&self, id: i64) -> moraebot::store::LaneRow {
        self.store
            .lock()
            .unwrap()
            .lane(id)
            .unwrap()
            .expect("lane exists")
    }
}

fn start_argv(line: &str) -> Vec<String> {
    let json = line.split_once("argv=").expect("argv in START line").1;
    serde_json::from_str(json).expect("argv json")
}

fn session_dir_arg(line: &str) -> String {
    let argv = start_argv(line);
    let at = argv
        .iter()
        .position(|a| a == "--session-dir")
        .expect("--session-dir");
    argv[at + 1].clone()
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("operation finished within bound")
}

fn lane_start(task: &str) -> Value {
    json!({"tool": lane::LANE_START_TOOL, "arguments": {"task": task}})
}

/// The `lane_id` in a successful `lane_start` result (JSON on its first line).
fn started_id(text: &str) -> i64 {
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).expect("json line");
    first["lane_id"].as_i64().expect("lane_id")
}

fn assert_refused(result: &(bool, String), code: &str) {
    assert!(result.0, "expected an error result: {result:?}");
    assert!(
        result.1.starts_with(&format!("{code}:")),
        "expected {code}: {result:?}"
    );
}

async fn await_running_pids(omp: &OmpManager) {
    let mut pids = omp.lanes().running_pids();
    bounded(pids.wait_for(|p| !p.is_empty()))
        .await
        .expect("pid channel open");
}

fn queued_row(h: &Harness, id: i64) -> moraebot::store::InboxRow {
    h.store
        .lock()
        .unwrap()
        .get(id)
        .unwrap()
        .expect("inbox row exists")
}

#[tokio::test]
async fn lane_start_returns_id_and_result_row_wakes_dispatcher() {
    let h = Harness::new();
    let omp = h.manager("ok", &[lane_start("t1")]);
    let wake = omp.lane_wake();
    let notified = wake.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();

    let report = bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    assert_eq!(
        report.main.outcome,
        Outcome::Reply("echo: run-tools go".into())
    );
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    assert_eq!(started_id(&results[0].1), 1);

    bounded(notified).await;
    assert_eq!(h.lane(1).state, LaneState::Idle);
    let row = queued_row(&h, 1);
    assert_eq!(row.platform, lane::LANE_PLATFORM);
    assert_eq!(row.message_id, "1:1");
    assert_eq!(row.origin, "lane");
    let tag = lead::parse_tag(&row.text).expect("tagged result");
    assert_eq!(tag.conversation, KEY);
    assert_eq!(tag.lane, Some(1));
    assert_eq!(tag.failed, None);
    assert_eq!(row.text.split_once('\n').unwrap().1, "echo: t1");

    let lines = h.log_lines();
    let starts = h.lane_starts();
    assert_eq!(starts.len(), 1);
    let argv = start_argv(&lines[starts[0]]);
    assert!(session_dir_arg(&lines[starts[0]]).ends_with("/lanes/1/session"));
    assert!(
        !argv.iter().any(|a| a.starts_with("--append-system-prompt")),
        "{argv:?}"
    );
    assert!(
        !lines[starts[0]..]
            .iter()
            .any(|l| l.contains(r#""type":"set_host_tools""#)),
        "a lane child gets no host tools"
    );
    omp.shutdown_all().await;
}

#[tokio::test]
async fn lane_start_does_not_wait_for_the_lane() {
    let h = Harness::new();
    let omp = h.manager("hang", &[lane_start("t1")]);
    let report = bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    assert_eq!(
        report.main.outcome,
        Outcome::Reply("echo: run-tools go".into())
    );
    assert_eq!(h.lane(1).state, LaneState::Running);
    await_running_pids(&omp).await;

    bounded(omp.shutdown_all()).await;
    assert!(omp.lanes().running_pids().borrow().is_empty());
    h.store.lock().unwrap().recover_on_start().unwrap();
    let lane = h.lane(1);
    assert_eq!(lane.state, LaneState::NeedsCheck);
    assert_eq!(lane.error.as_deref(), Some(E_LANE_INTERRUPTED));
    assert_eq!(h.store.lock().unwrap().count().unwrap(), 0, "no result row");
}

#[tokio::test]
async fn fourth_running_lane_is_refused() {
    let h = Harness::new();
    let calls: Vec<Value> = ["t1", "t2", "t3", "t4"].map(lane_start).to_vec();
    let omp = h.manager("hang", &calls);
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 4, "{results:?}");
    for (n, result) in results[..3].iter().enumerate() {
        assert!(!result.0, "{result:?}");
        assert_eq!(started_id(&result.1), n as i64 + 1);
    }
    assert_refused(&results[3], E_LANE_LIMIT);
    assert_eq!(h.store.lock().unwrap().lanes().unwrap().len(), 3);
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn lane_message_refused_while_running() {
    let h = Harness::new();
    let omp = h.manager(
        "hang",
        &[
            lane_start("t1"),
            json!({"tool": lane::LANE_MESSAGE_TOOL, "arguments": {"lane_id": 1, "text": "t2"}}),
        ],
    );
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    assert_refused(&results[1], E_LANE_BUSY);
    assert_eq!(h.lane(1).prompts, 1);
    bounded(omp.shutdown_all()).await;
    assert!(h.lane_starts().len() <= 1, "no second lane child");
}

#[tokio::test]
async fn lane_message_resumes_idle_lane_session() {
    let h = Harness::new();
    let m1 = h.manager("ok", &[lane_start("t1")]);
    let wake = m1.lane_wake();
    let notified = wake.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    bounded(m1.prompt(KEY, 1, "run-tools go")).await;
    bounded(notified).await;
    assert_eq!(h.lane(1).state, LaneState::Idle);
    assert_eq!(queued_row(&h, 1).message_id, "1:1");
    bounded(m1.shutdown_all()).await;
    assert!(m1.lanes().running_pids().borrow().is_empty());

    let m2 = h.manager(
        "ok",
        &[json!({"tool": lane::LANE_MESSAGE_TOOL, "arguments": {"lane_id": 1, "text": "t2"}})],
    );
    let wake = m2.lane_wake();
    let notified = wake.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    bounded(m2.prompt(KEY, 2, "run-tools again")).await;
    let results = h.tool_results();
    let last = results.last().expect("lane_message result");
    assert!(!last.0, "{results:?}");
    let accepted: Value = serde_json::from_str(&last.1).expect("json result");
    assert_eq!(accepted, json!({"lane_id": 1, "accepted": true}));
    bounded(notified).await;

    let second = queued_row(&h, 2);
    assert_eq!(second.message_id, "1:2");
    assert_eq!(second.text.split_once('\n').unwrap().1, "echo: t2");
    let lane = h.lane(1);
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(lane.prompts, 2);
    bounded(m2.shutdown_all()).await;
    assert!(m2.lanes().running_pids().borrow().is_empty());

    let session = lane.omp_session_id.expect("lane session stored");
    let lines = h.log_lines();
    let starts = h.lane_starts();
    assert_eq!(starts.len(), 2);
    let after: Vec<Value> = lines[starts[1]..]
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let lease = after
        .iter()
        .position(|f| f["type"] == "get_session_lease")
        .expect("lease checked before reopening");
    assert_eq!(after[lease]["sessionIds"], json!([session]));
    assert!(
        after[lease..].iter().any(|f| f["type"] == "open_session"),
        "the session is opened after the lease check"
    );
}

#[tokio::test]
async fn lane_status_lists_lanes_and_rejects_unknown() {
    let h = Harness::new();
    let long_task = "x".repeat(300);
    let status = |args: Value| json!({"tool": lane::LANE_STATUS_TOOL, "arguments": args});
    let omp = h.manager(
        "hang",
        &[
            lane_start("t1"),
            lane_start(&long_task),
            status(json!({})),
            status(json!({"lane_id": 1})),
            status(json!({"lane_id": 99})),
        ],
    );
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 5, "{results:?}");
    let all: Value = serde_json::from_str(&results[2].1).expect("status json");
    let all = all.as_array().expect("array");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["lane_id"], 1);
    assert_eq!(all[0]["conversation"], KEY);
    assert_eq!(all[0]["state"], "running");
    assert_eq!(all[0]["prompts"], 1);
    assert_eq!(all[0]["task"], "t1");
    assert_eq!(all[1]["task"].as_str().unwrap().chars().count(), 200);
    let one: Value = serde_json::from_str(&results[3].1).expect("status json");
    assert_eq!(one.as_array().unwrap().len(), 1);
    assert_eq!(one[0]["lane_id"], 1);
    assert_refused(&results[4], E_LANE_UNKNOWN);
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn lane_args_are_validated() {
    let h = Harness::new();
    let call = |tool: &str, args: Value| json!({"tool": tool, "arguments": args});
    let omp = h.manager(
        "ok",
        &[
            call(lane::LANE_START_TOOL, json!({"task": ""})),
            call(lane::LANE_START_TOOL, json!({})),
            call(lane::LANE_MESSAGE_TOOL, json!({"text": "x"})),
            call(lane::LANE_MESSAGE_TOOL, json!({"lane_id": 1})),
            call(lane::LANE_STATUS_TOOL, json!({"lane_id": "a"})),
        ],
    );
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 5, "{results:?}");
    for result in &results {
        assert_refused(result, E_LANE_ARGS);
    }
    assert!(h.store.lock().unwrap().lanes().unwrap().is_empty());
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn lane_start_after_shutdown_starts_no_child() {
    let h = Harness::new();
    let omp = h.manager("ok", &[lane_start("t1")]);
    bounded(omp.shutdown_all()).await;
    let report = bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    assert_eq!(
        report.main.outcome,
        Outcome::Reply("echo: run-tools go".into())
    );
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert_refused(&results[0], E_LANE_STOPPING);
    assert!(h.lane_starts().is_empty());
    assert!(h.store.lock().unwrap().lanes().unwrap().is_empty());
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn shutdown_while_lane_child_starts_kills_it() {
    let h = Harness::new();
    let omp = h.manager("hold_open", &[lane_start("t1")]);
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    await_running_pids(&omp).await;

    tokio::time::timeout(
        omp::ABORT_JOIN_BOUND + Duration::from_secs(5),
        omp.shutdown_all(),
    )
    .await
    .expect("shutdown finished");
    assert!(omp.lanes().running_pids().borrow().is_empty());
    assert_eq!(h.lane(1).state, LaneState::Running);
    assert_eq!(h.store.lock().unwrap().count().unwrap(), 0, "no result row");
    h.store.lock().unwrap().recover_on_start().unwrap();
    let lane = h.lane(1);
    assert_eq!(lane.state, LaneState::NeedsCheck);
    assert_eq!(lane.error.as_deref(), Some(E_LANE_INTERRUPTED));
}

#[tokio::test]
async fn lane_result_written_when_stop_did_not_kill_it() {
    let h = Harness::new();
    let omp = h.manager("ok", &[lane_start("t1")]);
    let wake = omp.lane_wake();
    let notified = wake.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    await_running_pids(&omp).await;
    omp.lanes().stop_accepting();
    bounded(notified).await;
    assert_eq!(h.lane(1).state, LaneState::Idle);
    assert_eq!(queued_row(&h, 1).message_id, "1:1");
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn lane_failure_comes_back_as_failed_tag() {
    let h = Harness::new();
    let omp = h.manager("error", &[lane_start("t1")]);
    let wake = omp.lane_wake();
    let notified = wake.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    bounded(notified).await;
    let row = queued_row(&h, 1);
    let tag = lead::parse_tag(&row.text).expect("tagged result");
    assert_eq!(tag.lane, Some(1));
    assert_eq!(tag.failed.as_deref(), Some(omp::E_PROMPT_ERROR));
    assert_eq!(h.lane(1).state, LaneState::Failed);
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn duplicate_tool_call_starts_one_lane() {
    let h = Harness::new();
    let dup = json!({"tool": lane::LANE_START_TOOL, "arguments": {"task": "t1"}, "id": "dup"});
    let omp = h.manager("hang", &[dup.clone(), dup]);
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(!results[0].0 && !results[1].0, "{results:?}");
    assert_eq!(started_id(&results[0].1), started_id(&results[1].1));
    assert_eq!(h.store.lock().unwrap().lanes().unwrap().len(), 1);
    await_running_pids(&omp).await;
    assert_eq!(omp.lanes().running_pids().borrow().len(), 1);
    bounded(omp.shutdown_all()).await;
}
