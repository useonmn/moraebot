//! fake-omp: a scripted stand-in for `omp --mode rpc`, used by contract tests.
//!
//! Reads JSONL commands on stdin and answers on stdout. The behaviour is
//! selected by FAKE_OMP_MODE (default `ok`). When FAKE_OMP_LOG is set, the
//! process appends a trace line at startup (`START pid=<pid> argv=<json>`),
//! a line with the environment variable NAMES only (`ENV <names>`), and then
//! every received stdin line.

use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};

const SESSION_ID: &str = "fake-session-1";

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

    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Some(path) = &log {
            log_line(path, &line);
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        handle(&mode, &frame, &mut out);
    }
}

fn handle(mode: &str, frame: &Value, out: &mut impl Write) {
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
        "prompt" => prompt(mode, &id, frame, out),
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
        _ => {
            let message = frame
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
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
    }
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
