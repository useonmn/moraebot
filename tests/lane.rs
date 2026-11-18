//! Lanes driven through a real `OmpManager` with fake-omp as lead (`FAKE_OMP_MODE=tools`)
//! and as lanes (`FAKE_OMP_LANE_MODE`). Only the lead prompt that should call tools
//! contains the trigger `run-tools`, so each test starts an exact number of lanes. Every
//! test ends with `shutdown_all`, which releases held lanes.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
use moraebot::done::{self, E_DONE_SYNTAX, E_DONE_UNMET};
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
    lane_start_done(task, "file .")
}

fn lane_start_done(task: &str, done: &str) -> Value {
    json!({"tool": lane::LANE_START_TOOL, "arguments": {"task": task, "done": done}})
}

/// A check command that appends its own pid to the fake-omp log and keeps that pid as
/// the group leader until it is killed.
const CHECK_ARGV: [&str; 3] = [
    "/bin/sh",
    "-c",
    "echo CHECK pid=$$ >> \"$FAKE_OMP_LOG\"; exec /bin/sleep 600",
];

fn check_done_line() -> String {
    format!("command {}", serde_json::to_string(&CHECK_ARGV).unwrap())
}

/// `n` scripted `lane_done` calls for a lane in mode `tools`.
fn lane_done_calls(n: usize) -> String {
    vec![json!({"tool": done::LANE_DONE_TOOL, "arguments": {"evidence": "e"}}).to_string(); n]
        .join("\n")
}

impl Harness {
    /// A manager whose lead starts one lane (mode `tools`, task with `run-lane-tools`)
    /// with `done`, and whose lane makes `calls` `lane_done` calls.
    fn done_manager(&self, done: &str, calls: usize) -> OmpManager {
        self.manager("tools", &[lane_start_done("run-lane-tools t1", done)])
            .with_extra_env([("FAKE_OMP_LANE_TOOL_CALLS", lane_done_calls(calls))])
    }

    fn marker(&self) -> PathBuf {
        self.config.work_dir.join("marker")
    }

    /// Results of the lane's `lane_done` calls, in log order: chosen by content, since
    /// the lead's lane tools answer with JSON.
    fn lane_tool_results(&self) -> Vec<(bool, String)> {
        self.tool_results()
            .into_iter()
            .filter(|(_, text)| text.starts_with(done::ACCEPTED) || text.starts_with("E_"))
            .collect()
    }

    /// Frames of `kind` logged after the lane `START` line number `nth`.
    fn frames_after_start(&self, nth: usize, kind: &str) -> Vec<Value> {
        let start = self.lane_starts()[nth];
        self.log_lines()[start..]
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|f| f["type"] == kind)
            .collect()
    }

    /// Pids the check commands logged (`CHECK pid=<pid>`).
    fn check_pids(&self) -> Vec<u32> {
        self.log_lines()
            .iter()
            .filter_map(|l| l.strip_prefix("CHECK pid="))
            .map(|pid| pid.trim().parse().expect("check pid"))
            .collect()
    }

    /// The pid in the lane `START` line number `nth`.
    fn lane_pid(&self, nth: usize) -> u32 {
        let line = &self.log_lines()[self.lane_starts()[nth]];
        let rest = line.strip_prefix("START pid=").expect("START line");
        rest.split_once(' ')
            .expect("pid then argv")
            .0
            .parse()
            .expect("pid")
    }
}

fn has_done_footer(frame: &Value, line: &str) -> bool {
    frame["message"]
        .as_str()
        .unwrap_or_default()
        .lines()
        .any(|l| l == format!("[moraebot] Done = {line}"))
}

fn process_exists(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Prompts the lead with `input` and waits until a lane result row was stored.
async fn prompt_until_lane_result(omp: &OmpManager, message: i64, input: &str) {
    let wake = omp.lane_wake();
    let notified = wake.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    bounded(omp.prompt(KEY, message, input)).await;
    bounded(notified).await;
}

fn result_tag(h: &Harness, row: i64) -> lead::Tag {
    lead::parse_tag(&queued_row(h, row).text).expect("tagged result")
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
    assert!(row.text.split_once('\n').unwrap().1.starts_with("echo: t1"));

    let lines = h.log_lines();
    let starts = h.lane_starts();
    assert_eq!(starts.len(), 1);
    let argv = start_argv(&lines[starts[0]]);
    assert!(session_dir_arg(&lines[starts[0]]).ends_with("/lanes/1/session"));
    assert!(
        !argv.iter().any(|a| a.starts_with("--append-system-prompt")),
        "{argv:?}"
    );
    let tools = h.frames_after_start(0, "set_host_tools");
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(tools[0]["tools"][0]["name"], done::LANE_DONE_TOOL);
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
    assert!(
        second
            .text
            .split_once('\n')
            .unwrap()
            .1
            .starts_with("echo: t2")
    );
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
            call(lane::LANE_START_TOOL, json!({"task": "", "done": "file ."})),
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
    let dup = json!({
        "tool": lane::LANE_START_TOOL,
        "arguments": {"task": "t1", "done": "file ."},
        "id": "dup"
    });
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

#[tokio::test]
async fn lane_start_requires_a_checkable_done_line() {
    let h = Harness::new();
    let start = |args: Value| json!({"tool": lane::LANE_START_TOOL, "arguments": args});
    let omp = h.manager(
        "ok",
        &[
            start(json!({"task": "t1"})),
            start(json!({"task": "t1", "done": "  "})),
            start(json!({"task": "t1", "done": "url http://127.0.0.1/"})),
        ],
    );
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    let results = h.tool_results();
    assert_eq!(results.len(), 3, "{results:?}");
    assert_refused(&results[0], E_LANE_ARGS);
    assert_refused(&results[1], E_LANE_ARGS);
    assert_refused(&results[2], E_DONE_SYNTAX);
    assert!(h.store.lock().unwrap().lanes().unwrap().is_empty());
    assert!(h.lane_starts().is_empty());
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn lane_with_done_line_gets_lane_done() {
    let h = Harness::new();
    let omp = h.manager("ok", &[lane_start_done("t1", "file .")]);
    prompt_until_lane_result(&omp, 1, "run-tools go").await;
    let tools = h.frames_after_start(0, "set_host_tools");
    assert_eq!(tools.len(), 1, "{tools:?}");
    let names: Vec<&Value> = tools[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| &t["name"])
        .collect();
    assert_eq!(names, [done::LANE_DONE_TOOL]);
    let prompts = h.frames_after_start(0, "prompt");
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(has_done_footer(&prompts[0], "file ."), "{prompts:?}");
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn lane_without_done_line_gets_no_tool() {
    let h = Harness::new();
    {
        let mut store = h.store.lock().unwrap();
        let started = store
            .start_lane("toolu_old", KEY, "t1", None, None, 3)
            .unwrap();
        assert_eq!(started, moraebot::store::LaneStart::New(1));
        let text = lead::lane_result(KEY, 1, &lead::LaneResult::Text("old"));
        assert!(
            store
                .finish_lane(1, 1, &text, LaneState::Idle, None)
                .unwrap()
        );
    }
    let omp = h.manager(
        "ok",
        &[json!({"tool": lane::LANE_MESSAGE_TOOL, "arguments": {"lane_id": 1, "text": "t2"}})],
    );
    prompt_until_lane_result(&omp, 1, "run-tools go").await;
    assert_eq!(h.lane_starts().len(), 1);
    let start = h.lane_starts()[0];
    assert!(
        !h.log_lines()[start..]
            .iter()
            .any(|l| l.contains(done::LANE_DONE_TOOL)),
        "a lane without a done line gets no lane_done"
    );
    let prompts = h.frames_after_start(0, "prompt");
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(
        !prompts[0]["message"]
            .as_str()
            .unwrap()
            .lines()
            .any(|l| l.starts_with("[moraebot] Done =")),
        "{prompts:?}"
    );
    let row = queued_row(&h, 2);
    assert_eq!(row.message_id, "1:2");
    let tag = lead::parse_tag(&row.text).expect("tagged result");
    assert!(!tag.done);
    assert_eq!(tag.failed, None);
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn accepted_claim_gives_done_tag_and_receipt() {
    let h = Harness::new();
    std::fs::write(h.marker(), "").unwrap();
    let omp = h.done_manager("file marker", 1);
    prompt_until_lane_result(&omp, 1, "run-tools go").await;
    let results = h.lane_tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].0, "{results:?}");
    let tag = result_tag(&h, 1);
    assert!(tag.done, "{tag:?}");
    assert_eq!(tag.failed, None);
    let lane = h.lane(1);
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(lane.error, None);
    assert_eq!(h.lane_starts().len(), 1);
    assert_eq!(h.frames_after_start(0, "prompt").len(), 1);
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn refused_claim_keeps_the_lane_working_until_it_passes() {
    let h = Harness::new();
    let omp = h.done_manager("file marker", 2).with_extra_env([(
        "FAKE_OMP_TOUCH_ON_TOOL_ERROR",
        h.marker().to_string_lossy().into_owned(),
    )]);
    prompt_until_lane_result(&omp, 1, "run-tools go").await;
    let results = h.lane_tool_results();
    assert_eq!(results.len(), 2, "{results:?}");
    assert_refused(&results[0], E_DONE_UNMET);
    assert!(
        results[0].1.contains("file marker -> E_CHECK_MISSING"),
        "{results:?}"
    );
    assert!(!results[1].0, "{results:?}");
    assert!(result_tag(&h, 1).done);
    assert_eq!(h.lane_starts().len(), 1);
    assert_eq!(
        h.frames_after_start(0, "prompt").len(),
        1,
        "moraebot sent no prompt of its own"
    );
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn no_limit_on_refused_claims() {
    let h = Harness::new();
    let omp = h.done_manager("file marker", 5).with_extra_env([
        (
            "FAKE_OMP_TOUCH_ON_TOOL_ERROR",
            h.marker().to_string_lossy().into_owned(),
        ),
        ("FAKE_OMP_TOUCH_AFTER_ERRORS", "4".to_owned()),
    ]);
    prompt_until_lane_result(&omp, 1, "run-tools go").await;
    let results = h.lane_tool_results();
    assert_eq!(results.len(), 5, "{results:?}");
    for result in &results[..4] {
        assert_refused(result, E_DONE_UNMET);
    }
    assert!(!results[4].0, "{results:?}");
    assert!(result_tag(&h, 1).done);
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn last_failed_claim_reports_unmet() {
    let h = Harness::new();
    let m1 = h.done_manager("file marker", 1);
    prompt_until_lane_result(&m1, 1, "run-tools go").await;
    let tag = result_tag(&h, 1);
    assert_eq!(tag.failed.as_deref(), Some(E_DONE_UNMET));
    let lane = h.lane(1);
    assert_eq!(lane.state, LaneState::Failed);
    assert_eq!(lane.error.as_deref(), Some(E_DONE_UNMET));
    assert_eq!(h.lane_starts().len(), 1);
    assert_eq!(h.frames_after_start(0, "prompt").len(), 1);
    bounded(m1.shutdown_all()).await;

    std::fs::write(h.marker(), "").unwrap();
    let m2 = h
        .manager(
            "tools",
            &[json!({
                "tool": lane::LANE_MESSAGE_TOOL,
                "arguments": {"lane_id": 1, "text": "run-lane-tools again"}
            })],
        )
        .with_extra_env([("FAKE_OMP_LANE_TOOL_CALLS", lane_done_calls(1))]);
    prompt_until_lane_result(&m2, 2, "run-tools again").await;
    assert!(result_tag(&h, 2).done);
    assert_eq!(h.lane(1).prompts, 2);
    let prompts = h.frames_after_start(1, "prompt");
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(has_done_footer(&prompts[0], "file marker"), "{prompts:?}");
    bounded(m2.shutdown_all()).await;
}

#[tokio::test]
async fn no_claim_gives_the_unchecked_result_tag() {
    let h = Harness::new();
    std::fs::write(h.marker(), "").unwrap();
    let omp = h.manager("ok", &[lane_start_done("t1", "file marker")]);
    prompt_until_lane_result(&omp, 1, "run-tools go").await;
    let tag = result_tag(&h, 1);
    assert!(!tag.done, "{tag:?}");
    assert_eq!(tag.failed, None);
    assert_eq!(h.lane(1).state, LaneState::Idle);
    assert!(h.lane_tool_results().is_empty());
    bounded(omp.shutdown_all()).await;
}

/// Time-limit exception: the 1 s check timeout is the behavior under test.
#[tokio::test]
async fn check_timeout_refuses_the_claim() {
    let h = Harness::new();
    let omp = h
        .done_manager(&check_done_line(), 1)
        .with_check_timeout(Duration::from_secs(1));
    prompt_until_lane_result(&omp, 1, "run-tools go").await;
    let results = h.lane_tool_results();
    assert_eq!(results.len(), 1, "{results:?}");
    assert_refused(&results[0], E_DONE_UNMET);
    assert!(results[0].1.contains("-> E_CHECK_TIMEOUT:"), "{results:?}");
    assert_eq!(result_tag(&h, 1).failed.as_deref(), Some(E_DONE_UNMET));
    for pid in h.check_pids() {
        assert!(!process_exists(pid), "check {pid} was reaped");
    }
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn shutdown_during_a_check_kills_it_and_writes_no_result() {
    let h = Harness::new();
    let omp = h.done_manager(&check_done_line(), 1);
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    let mut pids = omp.lanes().running_pids();
    let running = bounded(pids.wait_for(|p| p.len() == 2))
        .await
        .expect("pid channel open")
        .clone();
    let lane_pid = h.lane_pid(0);
    assert!(running.contains(&lane_pid), "{running:?}");
    let check_pid = *running.iter().find(|p| **p != lane_pid).unwrap();

    tokio::time::timeout(
        omp::ABORT_JOIN_BOUND + Duration::from_secs(5),
        omp.shutdown_all(),
    )
    .await
    .expect("shutdown finished");
    assert!(
        !process_exists(check_pid),
        "the check was killed and reaped"
    );
    assert!(omp.lanes().running_pids().borrow().is_empty());
    assert_eq!(h.store.lock().unwrap().count().unwrap(), 0, "no result row");
    assert_eq!(h.lane(1).state, LaneState::Running);
    h.store.lock().unwrap().recover_on_start().unwrap();
    let lane = h.lane(1);
    assert_eq!(lane.state, LaneState::NeedsCheck);
    assert_eq!(lane.error.as_deref(), Some(E_LANE_INTERRUPTED));
}

/// Time-limit exception: the 3 s turn timeout is the behavior under test.
#[tokio::test]
async fn turn_timeout_during_a_check_kills_the_check() {
    let h = Harness::new();
    let omp = h
        .done_manager(&check_done_line(), 1)
        .with_turn_timeout(Duration::from_secs(3));
    prompt_until_lane_result(&omp, 1, "run-tools go").await;
    assert_eq!(
        result_tag(&h, 1).failed.as_deref(),
        Some(omp::E_TURN_TIMEOUT)
    );
    let mut pids = omp.lanes().running_pids();
    bounded(pids.wait_for(|p| p.is_empty()))
        .await
        .expect("pid channel open");
    for pid in h.check_pids() {
        assert!(!process_exists(pid), "check {pid} was killed");
    }
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
async fn lane_status_lists_done_line() {
    let h = Harness::new();
    let omp = h.manager(
        "hang",
        &[
            lane_start_done("t1", "file marker"),
            json!({"tool": lane::LANE_STATUS_TOOL, "arguments": {}}),
        ],
    );
    bounded(omp.prompt(KEY, 1, "run-tools go")).await;
    let results = h.tool_results();
    let status = results
        .iter()
        .find(|(_, text)| text.starts_with('['))
        .expect("lane_status result");
    let listed: Value = serde_json::from_str(&status.1).expect("status json");
    assert_eq!(listed[0]["lane_id"], 1);
    assert_eq!(listed[0]["done"], "file marker");
    bounded(omp.shutdown_all()).await;
}
