//! fake-omp: a scripted stand-in for `omp --mode rpc`, used by contract tests.
//!
//! Reads JSONL commands on stdin and answers on stdout. The behaviour is
//! selected by FAKE_OMP_MODE (default `ok`). When FAKE_OMP_LOG is set, the
//! process appends a trace line at startup (`START pid=<pid> argv=<json>`),
//! a line with the environment variable NAMES only (`ENV <names>`), and then
//! every received stdin line.
//!
//! Host tools: `set_host_tools` is acknowledged in every mode with the tool
//! names of the frame. Mode `send_file` answers a prompt with one
//! `host_tool_call` for `send_file` per line of FAKE_OMP_SEND_FILE (unset or
//! empty: one call with an empty path). Each next call, and the final reply,
//! is written only after the matching `host_tool_result` frame arrived on
//! stdin, so no transition depends on time. With FAKE_OMP_LOG set, each call
//! also leaves a `CALL host_<n>` line just before it is written, which orders
//! the calls against the received `host_tool_result` lines.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

const SESSION_ID: &str = "fake-session-1";

/// A `send_file` turn in progress: the prompt it answers and the paths still
/// to be offered to the host.
struct SendFileTurn {
    prompt_id: Value,
    message: String,
    remaining: VecDeque<String>,
    calls_made: usize,
}

fn main() {
    let mode = std::env::var("FAKE_OMP_MODE").unwrap_or_else(|_| "ok".to_owned());
    let log = std::env::var_os("FAKE_OMP_LOG").map(std::path::PathBuf::from);
    if let Some(path) = &log {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let argv = serde_json::to_string(&argv).expect("argv is serializable");
        log_line(
            path,
            &format!("START pid={} argv={argv}", std::process::id()),
        );
        let mut names: Vec<String> = std::env::vars().map(|(name, _)| name).collect();
        names.sort();
        log_line(path, &format!("ENV {}", names.join(" ")));
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    send(
        &mut out,
        &json!({
            "type": "ready",
            "protocolVersion": 1,
            "supportedProtocolVersions": [1, 2],
            "maxFrameBytes": 1_048_576,
            "maxReassembledFrameBytes": 67_108_864,
        }),
    );
    send(
        &mut out,
        &json!({"type": "available_commands_update", "commands": []}),
    );

    let mut turn: Option<SendFileTurn> = None;
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Some(path) = &log {
            log_line(path, &line);
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        handle(&mode, &frame, &mut out, &mut turn);
    }
}

fn handle(mode: &str, frame: &Value, out: &mut impl Write, turn: &mut Option<SendFileTurn>) {
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    let command = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match command {
        "negotiate_protocol" => respond_ok(out, &id, command, json!({"protocolVersion": 2})),
        "open_session" if mode == "open_session_fail" => respond_err(
            out,
            &id,
            command,
            "open_session refused by fake-omp",
            "E_OPEN_REFUSED",
        ),
        "open_session" => respond_ok(
            out,
            &id,
            command,
            json!({
                "cancelled": false,
                "resumed": false,
                "sessionId": SESSION_ID,
                "sessionFile": "fake-session.jsonl",
            }),
        ),
        "get_state" => respond_ok(out, &id, command, json!({"sessionId": SESSION_ID})),
        "get_session_lease" => {
            let requested = frame
                .get("sessionIds")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let (held, unknown) = if mode == "lease_held" {
                (requested, json!([]))
            } else {
                (json!([]), requested)
            };
            respond_ok(out, &id, command, json!({"held": held, "unknown": unknown}));
        }
        "set_host_tools" => {
            let names: Vec<Value> = frame
                .get("tools")
                .and_then(Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool.get("name"))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            respond_ok(out, &id, command, json!({"toolNames": names}));
        }
        "prompt" if mode == "send_file" => start_send_file(&id, frame, out, turn),
        "prompt" => prompt(mode, &id, frame, out),
        "host_tool_result" => {
            if let Some(active) = turn.take() {
                *turn = advance_send_file(active, out);
            }
        }
        _ => respond_err(out, &id, command, "unknown command", "E_UNKNOWN_COMMAND"),
    }
}

fn prompt(mode: &str, id: &Value, frame: &Value, out: &mut impl Write) {
    if mode == "not_invoked" {
        respond_ok(out, id, "prompt", json!({"agentInvoked": false}));
        return;
    }
    respond_ok(out, id, "prompt", json!({"agentInvoked": true}));
    send(out, &json!({"type": "agent_start"}));
    match mode {
        "hang" => {}
        "exit1" => std::process::exit(1),
        "error" => {
            send(out, &json!({"type": "agent_end", "messages": []}));
            send(
                out,
                &json!({
                    "type": "prompt_result",
                    "id": id,
                    "agentInvoked": true,
                    "status": "error",
                    "sessionSettled": true,
                    "error": {"message": "fake provider failure", "retryable": false},
                }),
            );
        }
        _ => finish_echo(id, prompt_message(frame), out),
    }
}

fn prompt_message(frame: &Value) -> &str {
    frame
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn finish_echo(id: &Value, message: &str, out: &mut impl Write) {
    let assistant = json!({
        "role": "assistant",
        "content": [{"type": "text", "text": format!("echo: {message}")}],
        "stopReason": "stop",
        "timestamp": 0,
    });
    send(out, &json!({"type": "message_end", "message": assistant}));
    send(out, &json!({"type": "agent_end", "messages": [assistant]}));
    send(
        out,
        &json!({
            "type": "prompt_result",
            "id": id,
            "agentInvoked": true,
            "status": "completed",
            "sessionSettled": true,
        }),
    );
}

fn start_send_file(
    id: &Value,
    frame: &Value,
    out: &mut impl Write,
    turn: &mut Option<SendFileTurn>,
) {
    respond_ok(out, id, "prompt", json!({"agentInvoked": true}));
    send(out, &json!({"type": "agent_start"}));
    let script = std::env::var("FAKE_OMP_SEND_FILE").unwrap_or_default();
    let remaining: VecDeque<String> = if script.is_empty() {
        VecDeque::from([String::new()])
    } else {
        script.lines().map(str::to_owned).collect()
    };
    *turn = advance_send_file(
        SendFileTurn {
            prompt_id: id.clone(),
            message: prompt_message(frame).to_owned(),
            remaining,
            calls_made: 0,
        },
        out,
    );
}

/// Emits the next `send_file` call, or finishes the turn when none is left.
/// Returns the turn while it still waits for a `host_tool_result`.
fn advance_send_file(mut turn: SendFileTurn, out: &mut impl Write) -> Option<SendFileTurn> {
    let Some(path) = turn.remaining.pop_front() else {
        finish_echo(&turn.prompt_id, &turn.message, out);
        return None;
    };
    turn.calls_made += 1;
    let n = turn.calls_made;
    if let Some(path) = std::env::var_os("FAKE_OMP_LOG") {
        log_line(Path::new(&path), &format!("CALL host_{n}"));
    }
    send(
        out,
        &json!({
            "type": "host_tool_call",
            "id": format!("host_{n}"),
            "toolCallId": format!("toolu_{n}"),
            "toolName": "send_file",
            "arguments": {"path": path},
        }),
    );
    Some(turn)
}

fn respond_ok(out: &mut impl Write, id: &Value, command: &str, data: Value) {
    send(
        out,
        &json!({"id": id, "type": "response", "command": command, "success": true, "data": data}),
    );
}

fn respond_err(out: &mut impl Write, id: &Value, command: &str, error: &str, code: &str) {
    send(
        out,
        &json!({
            "id": id,
            "type": "response",
            "command": command,
            "success": false,
            "error": error,
            "code": code,
        }),
    );
}

fn send(out: &mut impl Write, frame: &Value) {
    writeln!(out, "{frame}")
        .and_then(|()| out.flush())
        .expect("write frame to stdout");
}

fn log_line(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open FAKE_OMP_LOG");
    writeln!(file, "{line}").expect("write FAKE_OMP_LOG");
}
