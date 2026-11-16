use std::path::Path;
use std::process::Command;
use std::time::Duration;

use omp_rpc::{
    Client, ClientOptions, DEFAULT_PROMPT_TIMEOUT, Error, GetSessionLeaseCommand,
    OpenSessionCommand, PromptCommand, PromptStatus,
};

fn spawn_fake(mode: &str, log: Option<&Path>) -> Client {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fake-omp"));
    command
        .args(["--mode", "rpc", "--no-ui"])
        .env("FAKE_OMP_MODE", mode);
    if let Some(log) = log {
        command.env("FAKE_OMP_LOG", log);
    }
    let (client, _events) =
        Client::spawn(command, ClientOptions::default()).expect("spawn fake-omp");
    client
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
