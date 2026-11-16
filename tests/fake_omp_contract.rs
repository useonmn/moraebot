use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use omp_rpc::{
    Client, ClientOptions, Error, GetSessionLeaseCommand, HostTool, HostToolError,
    OpenSessionCommand, PromptCommand, PromptStatus, DEFAULT_PROMPT_TIMEOUT,
};
use serde_json::{json, Map, Value};

const WAIT: Duration = Duration::from_secs(30);

fn spawn_fake(mode: &str, log: Option<&Path>) -> Client {
    spawn_fake_with(mode, log, None, Vec::new())
}

fn spawn_fake_with(
    mode: &str,
    log: Option<&Path>,
    send_file_paths: Option<&str>,
    tools: Vec<HostTool>,
) -> Client {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fake-omp"));
    command
        .args(["--mode", "rpc", "--no-ui"])
        .env("FAKE_OMP_MODE", mode);
    if let Some(log) = log {
        command.env("FAKE_OMP_LOG", log);
    }
    if let Some(paths) = send_file_paths {
        command.env("FAKE_OMP_SEND_FILE", paths);
    }
    let options = ClientOptions {
        tools,
        ..ClientOptions::default()
    };
    let (client, _events) = Client::spawn(command, options).expect("spawn fake-omp");
    client
}

/// A `send_file` host tool that reports every `path` argument on the returned
/// channel and answers with `outcome`.
fn send_file_tool(
    outcome: fn() -> Result<String, HostToolError>,
) -> (HostTool, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let parameters: Map<String, Value> = serde_json::from_value(json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
    }))
    .expect("schema object");
    let tool = HostTool::new(
        "send_file",
        "Send a file to the owner.",
        parameters,
        move |arguments, _context| {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            tx.lock()
                .expect("sender lock")
                .send(path)
                .expect("report path");
            outcome().map(Into::into)
        },
    );
    (tool, rx)
}

fn queued() -> Result<String, HostToolError> {
    Ok("Queued.".to_owned())
}

fn refused() -> Result<String, HostToolError> {
    Err("E_FILE_PATH: refused".into())
}

fn log_frames(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .expect("read log")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn host_tool_results(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|frame| frame["type"] == "host_tool_result")
        .collect()
}

fn open_session_command() -> OpenSessionCommand {
    OpenSessionCommand {
        session_dir: "fake-session-dir".to_owned(),
        provider: None,
        model_id: None,
    }
}

fn prompt(message: &str) -> PromptCommand {
    PromptCommand {
        message: message.to_owned(),
        images: None,
        streaming_behavior: None,
    }
}

#[test]
fn negotiates_protocol_v2() {
    let client = spawn_fake("ok", None);
    assert_eq!(client.protocol_version(), 2);
}

#[test]
fn open_session_succeeds() {
    let client = spawn_fake("ok", None);
    let opened = client.call(&open_session_command()).expect("open_session");
    assert_eq!(opened.session_id, "fake-session-1");
}

#[test]
fn open_session_failure_surfaces_error_code() {
    let client = spawn_fake("open_session_fail", None);
    match client.call(&open_session_command()) {
        Err(Error::Command { code, .. }) => assert_eq!(code.as_deref(), Some("E_OPEN_REFUSED")),
        other => panic!("expected a command error, got {other:?}"),
    }
}

#[test]
fn prompt_returns_assistant_text() {
    let client = spawn_fake("ok", None);
    let turn = client
        .prompt_and_wait(&prompt("hi"), DEFAULT_PROMPT_TIMEOUT)
        .expect("prompt");
    let result = turn.result.expect("prompt result");
    assert!(matches!(result.status, PromptStatus::Completed));
    assert_eq!(turn.assistant_text.as_deref(), Some("echo: hi"));
}

#[test]
fn not_invoked_prompt_has_no_result() {
    let client = spawn_fake("not_invoked", None);
    let turn = client
        .prompt_and_wait(&prompt("hi"), DEFAULT_PROMPT_TIMEOUT)
        .expect("prompt");
    assert!(turn.result.is_none());
    assert!(turn.assistant_text.is_none());
}

#[test]
fn error_prompt_reports_error_status() {
    let client = spawn_fake("error", None);
    let turn = client
        .prompt_and_wait(&prompt("hi"), DEFAULT_PROMPT_TIMEOUT)
        .expect("prompt");
    let result = turn.result.expect("prompt result");
    assert!(matches!(result.status, PromptStatus::Error));
}

#[test]
fn hung_prompt_times_out() {
    let client = spawn_fake("hang", None);
    match client.prompt_and_wait(&prompt("hi"), Duration::from_secs(2)) {
        Err(Error::Timeout { command }) => assert_eq!(command, "prompt"),
        other => panic!("expected a prompt timeout, got {other:?}"),
    }
}

#[test]
fn lease_reports_requested_sessions_as_not_held() {
    let client = spawn_fake("ok", None);
    let lease = client
        .call(&GetSessionLeaseCommand {
            session_ids: vec!["111".to_owned(), "222".to_owned()],
        })
        .expect("get_session_lease");
    assert!(lease.held.is_empty());
}

#[test]
fn lease_reports_requested_sessions_as_held() {
    let client = spawn_fake("lease_held", None);
    let lease = client
        .call(&GetSessionLeaseCommand {
            session_ids: vec!["111".to_owned()],
        })
        .expect("get_session_lease");
    assert_eq!(lease.held, vec!["111".to_owned()]);
}

#[test]
fn host_tools_registered() {
    let (tool, _rx) = send_file_tool(queued);
    let client = spawn_fake("send_file", None);
    let names = client.set_custom_tools(vec![tool]).expect("set_host_tools");
    assert_eq!(names, vec!["send_file".to_owned()]);
}

#[test]
fn send_file_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("fake-omp.log");
    let (tool, calls) = send_file_tool(queued);
    let client = spawn_fake_with("send_file", Some(&log), Some("report.txt"), vec![tool]);
    let turn = client
        .prompt_and_wait(&prompt("hi"), DEFAULT_PROMPT_TIMEOUT)
        .expect("prompt");

    assert_eq!(calls.recv_timeout(WAIT).expect("tool call"), "report.txt");
    assert!(calls.try_recv().is_err(), "exactly one call");
    let result = turn.result.expect("prompt result");
    assert!(matches!(result.status, PromptStatus::Completed));
    assert_eq!(turn.assistant_text.as_deref(), Some("echo: hi"));

    let lines = log_frames(&log);
    assert!(lines
        .iter()
        .any(|line| line.contains(r#""type":"set_host_tools""#)));
    let results = host_tool_results(&lines);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"], "host_1");
    assert!(results[0]["isError"].is_null());
}

#[test]
fn send_file_calls_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("fake-omp.log");
    let (tool, calls) = send_file_tool(queued);
    let client = spawn_fake_with("send_file", Some(&log), Some("a.txt\nb.txt"), vec![tool]);
    client
        .prompt_and_wait(&prompt("hi"), DEFAULT_PROMPT_TIMEOUT)
        .expect("prompt");

    assert_eq!(calls.recv_timeout(WAIT).expect("first call"), "a.txt");
    assert_eq!(calls.recv_timeout(WAIT).expect("second call"), "b.txt");

    // The fake logs `CALL host_<n>` right before writing a call and every
    // stdin line it reads, so the log order shows that call 2 was written
    // only after result 1 was read.
    let order: Vec<String> = log_frames(&log)
        .iter()
        .filter_map(|line| {
            if let Some(call) = line.strip_prefix("CALL ") {
                return Some(format!("call {call}"));
            }
            let frame = serde_json::from_str::<Value>(line).ok()?;
            (frame["type"] == "host_tool_result")
                .then(|| format!("result {}", frame["id"].as_str().unwrap_or_default()))
        })
        .collect();
    assert_eq!(
        order,
        [
            "call host_1",
            "result host_1",
            "call host_2",
            "result host_2"
        ]
    );
}

#[test]
fn send_file_error_result_is_logged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("fake-omp.log");
    let (tool, _calls) = send_file_tool(refused);
    let client = spawn_fake_with("send_file", Some(&log), Some("../outside"), vec![tool]);
    let turn = client
        .prompt_and_wait(&prompt("hi"), DEFAULT_PROMPT_TIMEOUT)
        .expect("prompt");

    let result = turn.result.expect("prompt result");
    assert!(matches!(result.status, PromptStatus::Completed));
    let results = host_tool_results(&log_frames(&log));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["isError"], true);
}

#[test]
fn log_records_startup_argv_env_names_and_received_frames() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("fake-omp.log");
    let client = spawn_fake("ok", Some(&log));
    client
        .prompt_and_wait(&prompt("hi"), DEFAULT_PROMPT_TIMEOUT)
        .expect("prompt");

    let text = std::fs::read_to_string(&log).expect("read log");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("START pid="))
            .count(),
        1
    );
    let start = lines
        .iter()
        .find(|line| line.starts_with("START pid="))
        .expect("START line");
    assert!(start.contains(r#"argv=["--mode","rpc","--no-ui"]"#));
    let env = lines
        .iter()
        .find(|line| line.starts_with("ENV "))
        .expect("ENV line");
    let names: Vec<&str> = env.trim_start_matches("ENV ").split_whitespace().collect();
    assert!(names.contains(&"FAKE_OMP_MODE"));
    assert!(!env.contains('='));
    assert!(lines.iter().any(|line| line.contains(r#""type":"prompt""#)));
}
