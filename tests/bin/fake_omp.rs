//! fake-omp: a scripted stand-in for `omp --mode rpc`, used by contract tests.
//!
//! Reads JSONL commands on stdin and answers on stdout. The behaviour is
//! selected by FAKE_OMP_MODE (default `ok`). When the `--session-dir` argument
//! contains `/lanes/`, FAKE_OMP_LANE_MODE (default `ok`) is used instead, so a
//! lead and its lanes run different scripts from one environment. When
//! FAKE_OMP_LOG is set, the process appends a trace line at startup
//! (`START pid=<pid> argv=<json>`), a line with the environment variable NAMES
//! only (`ENV <names>`), and then every received stdin line. When
//! PI_CONFIG_FILES is set, a `CONFIG_FILES <value>` line follows the `ENV` line.
//!
//! Envelope: a prompt whose first line starts with `[moraebot ` and ends with
//! `]` is echoed without that line (and its newline); any other message is
//! echoed whole. The log keeps the full frame.
//!
//! Host tools: `set_host_tools` is acknowledged in every mode with the tool
//! names of the frame. Mode `send_file` answers a prompt with one
//! `host_tool_call` for `send_file` per line of FAKE_OMP_SEND_FILE (unset or
//! empty: one call with an empty path). Mode `tools` answers a prompt whose
//! body contains FAKE_OMP_TOOL_TRIGGER (default `run-tools`) with one
//! `host_tool_call` per JSON line of FAKE_OMP_TOOL_CALLS
//! (`{"tool": "<name>", "arguments": {...}, "id": "<toolCallId>"}`; without
//! `id` the call id is `toolu_<pid>_<n>`, `n` counting calls over the child's
//! life); every other prompt is answered like `ok`. In both modes each next
//! call, and the final reply, is written only after the matching
//! `host_tool_result` frame arrived on stdin, so no transition depends on
//! time. With FAKE_OMP_LOG set, each call also leaves a `CALL host_<n>` line
//! just before it is written, which orders the calls against the received
//! `host_tool_result` lines.
//!
//! Lane tool script: a lane fake (its `--session-dir` contains `/lanes/`) in
//! mode `tools` reads FAKE_OMP_LANE_TOOL_TRIGGER (default `run-lane-tools`) and
//! FAKE_OMP_LANE_TOOL_CALLS (same line format as FAKE_OMP_TOOL_CALLS) instead of
//! FAKE_OMP_TOOL_TRIGGER and FAKE_OMP_TOOL_CALLS, which only a lead fake reads.
//!
//! Touch on a refused call: FAKE_OMP_TOUCH_ON_TOOL_ERROR=<path> with
//! FAKE_OMP_TOUCH_AFTER_ERRORS=<n> (default 1) creates `<path>` empty (parents
//! are not created) when the n-th `host_tool_result` frame with
//! `"isError": true` arrives, counted over the child's life, before the next
//! call or the final reply is written. It applies to any fake, lead or lane.
//!
//! Steer: in mode `steer` the first prompt answers `agent_start` and waits. A
//! later prompt with `"streamingBehavior":"steer"` is acknowledged and handled
//! by FAKE_OMP_STEER_CASE: `merged` (one run answers both), `separate` (two
//! runs, one answer each), `hold` (writes `{"type":"fake_held",...}` and then
//! nothing more), anything else (an error `prompt_result` for both). Other
//! prompts are answered like `ok`; other modes answer a steer frame like any
//! prompt.
//!
//! Lane mode `hold_open` answers `open_session` with a `fake_held` marker
//! instead of a response and then writes nothing more. A held fake is released
//! only by stdin closing or a kill.
//!
//! Compaction and settle frames (lead fakes only; a lane fake ignores them):
//! when a prompt's message (envelope line stripped) contains
//! FAKE_OMP_COMPACT_TRIGGER (unset: never), one compaction frame follows that
//! prompt's `agent_start`: `auto_compaction_end` for FAKE_OMP_COMPACT `auto`
//! (default), `manual_compaction_end` for `manual`, an `aborted` one without a
//! result for `aborted`, a `skipped` one without a result for `skipped`. A
//! prompt whose message contains FAKE_OMP_UNSETTLED_TRIGGER (unset: never)
//! gets a `prompt_result` with `"sessionSettled": false`; with
//! FAKE_OMP_SETTLE_FRAME=1 a `session_settled` frame follows it at once.
//!
//! Memory extension: when argv holds the element `-e`, FAKE_OMP_EXT_ACK is set
//! and the extension's home variable (`extension::HOME_ENV`) is set, the fake
//! writes `<home>/omp/<ACK_FILE>` = `{"pid": <own pid>, "version":
//! <FAKE_OMP_EXT_ACK>}` (temp file and rename, mode 0o600) before `ready`, and
//! logs `EXT_ACK <value>`. A fake that wrote the ack then plays the
//! extension's outbox at every received `prompt` frame (main or steer), after
//! logging the frame and before answering it: the non-dot `.md` entries of
//! `<home>/omp/<OUTBOX_DIR>` are read in ascending name order and deleted,
//! their texts are joined and held as pending for that frame's `message`, and a
//! later attempt with the same `message` returns the held text again; a
//! non-empty accepted text is logged as `OUTBOX <json string>`. An outbox that
//! cannot be listed drains nothing. With FAKE_OMP_HOOK_RETRY=1 the step runs
//! twice per frame and only the second attempt is logged. Answering the frame
//! clears the held text.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use moraebot::memory::extension::{ACK_FILE, HOME_ENV, OUTBOX_DIR};
use serde_json::{Value, json};

const SESSION_ID: &str = "fake-session-1";
const DEFAULT_TOOL_TRIGGER: &str = "run-tools";
const DEFAULT_LANE_TOOL_TRIGGER: &str = "run-lane-tools";

/// One host tool call still to be offered to the host.
struct PlannedCall {
    tool: String,
    arguments: Value,
    tool_call_id: Option<String>,
}

/// A host-tool turn in progress: the prompt it answers and the calls still to
/// be offered to the host.
struct ToolTurn {
    prompt_id: Value,
    message: String,
    remaining: VecDeque<PlannedCall>,
    calls_made: usize,
}

/// The main prompt of a `steer` turn, waiting for its steer.
struct SteerTurn {
    prompt_id: Value,
    message: String,
}

struct Fake {
    mode: String,
    /// The `--session-dir` contains `/lanes/`: this fake is a lane child.
    lane: bool,
    /// `host_tool_result` frames with `isError` true seen over the child's life.
    tool_errors: usize,
    /// Host tool calls written over the child's life (default call ids).
    total_calls: usize,
    tool_turn: Option<ToolTurn>,
    steer_turn: Option<SteerTurn>,
    /// `steer` mode: the first prompt was taken.
    first_prompt_seen: bool,
    /// Set by a `hold` case: nothing more is written.
    held: bool,
    /// `<home>/omp` when this fake wrote the extension ack.
    ext_dir: Option<PathBuf>,
    /// Outbox text drained for a prompt and not yet confirmed by its answer.
    pending: Option<Pending>,
}

/// Drained outbox text and the `message` of the prompt it was drained for.
struct Pending {
    prompt: String,
    text: String,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let lane = is_lane(&argv);
    let mode = select_mode(lane);
    let log = std::env::var_os("FAKE_OMP_LOG").map(std::path::PathBuf::from);
    if let Some(path) = &log {
        let argv = serde_json::to_string(&argv).expect("argv is serializable");
        log_line(
            path,
            &format!("START pid={} argv={argv}", std::process::id()),
        );
        let mut names: Vec<String> = std::env::vars().map(|(name, _)| name).collect();
        names.sort();
        log_line(path, &format!("ENV {}", names.join(" ")));
        if let Some(value) = std::env::var_os("PI_CONFIG_FILES") {
            log_line(path, &format!("CONFIG_FILES {}", value.to_string_lossy()));
        }
    }
    let ext_dir = write_ext_ack(&argv, log.as_deref());

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

    let mut fake = Fake {
        mode,
        lane,
        tool_errors: 0,
        total_calls: 0,
        tool_turn: None,
        steer_turn: None,
        first_prompt_seen: false,
        held: false,
        ext_dir,
        pending: None,
    };
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Some(path) = &log {
            log_line(path, &line);
        }
        if fake.held {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let is_prompt = frame.get("type").and_then(Value::as_str) == Some("prompt");
        if is_prompt {
            fake.play_outbox(&frame, log.as_deref());
        }
        fake.handle(&frame, &mut out);
        if is_prompt {
            fake.pending = None;
        }
    }
}

/// Writes the extension ack when argv holds `-e` and both FAKE_OMP_EXT_ACK and
/// the extension's home variable are set; returns `<home>/omp` when it did.
fn write_ext_ack(argv: &[String], log: Option<&Path>) -> Option<PathBuf> {
    if !argv.iter().any(|arg| arg == "-e") {
        return None;
    }
    let version = std::env::var("FAKE_OMP_EXT_ACK").ok()?;
    let dir = PathBuf::from(std::env::var_os(HOME_ENV)?).join("omp");
    let ack = json!({"pid": std::process::id(), "version": version});
    let tmp = dir.join(format!(".{ACK_FILE}.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .expect("create the ack temp file");
    file.write_all(ack.to_string().as_bytes())
        .expect("write the ack temp file");
    std::fs::rename(&tmp, dir.join(ACK_FILE)).expect("rename the ack file");
    if let Some(path) = log {
        log_line(path, &format!("EXT_ACK {version}"));
    }
    Some(dir)
}

/// The text of every non-dot `.md` entry of `dir`, in ascending name order,
/// each deleted after reading. An outbox that cannot be listed drains nothing.
fn drain(dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".md") && !name.starts_with('.'))
        .collect();
    names.sort();
    let mut text = String::new();
    for name in names {
        let path = dir.join(&name);
        if let Ok(entry) = std::fs::read_to_string(&path) {
            text.push_str(&entry);
        }
        let _ = std::fs::remove_file(&path);
    }
    text
}

/// Whether the `--session-dir` argument contains `/lanes/` (a lane child).
fn is_lane(argv: &[String]) -> bool {
    let session_dir = argv.iter().enumerate().find_map(|(index, arg)| {
        if arg == "--session-dir" {
            argv.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--session-dir=")
        }
    });
    session_dir.is_some_and(|dir| dir.contains("/lanes/"))
}

/// FAKE_OMP_LANE_MODE for a lane child, FAKE_OMP_MODE otherwise.
fn select_mode(lane: bool) -> String {
    let variable = if lane {
        "FAKE_OMP_LANE_MODE"
    } else {
        "FAKE_OMP_MODE"
    };
    std::env::var(variable).unwrap_or_else(|_| "ok".to_owned())
}

impl Fake {
    /// The extension's outbox step for one `prompt` frame (twice with
    /// FAKE_OMP_HOOK_RETRY=1, the first attempt discarded); logs the accepted
    /// attempt's non-empty text as `OUTBOX <json string>`.
    fn play_outbox(&mut self, frame: &Value, log: Option<&Path>) {
        let Some(dir) = self.ext_dir.clone() else {
            return;
        };
        let message = frame
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if std::env::var("FAKE_OMP_HOOK_RETRY").as_deref() == Ok("1") {
            self.outbox_attempt(&dir, message);
        }
        let text = self.outbox_attempt(&dir, message);
        if let (false, Some(path)) = (text.is_empty(), log) {
            let value = serde_json::to_string(&text).expect("text is serializable");
            log_line(path, &format!("OUTBOX {value}"));
        }
    }

    /// One `before_agent_start` attempt: the held text for the same `message`
    /// plus a fresh drain, held again when non-empty.
    fn outbox_attempt(&mut self, dir: &Path, message: &str) -> String {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.prompt != message)
        {
            self.pending = None;
        }
        let mut text = self
            .pending
            .take()
            .map(|pending| pending.text)
            .unwrap_or_default();
        text.push_str(&drain(&dir.join(OUTBOX_DIR)));
        if !text.is_empty() {
            self.pending = Some(Pending {
                prompt: message.to_owned(),
                text: text.clone(),
            });
        }
        text
    }

    fn handle(&mut self, frame: &Value, out: &mut impl Write) {
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let command = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mode = self.mode.as_str();
        match command {
            "negotiate_protocol" => respond_ok(out, &id, command, json!({"protocolVersion": 2})),
            "open_session" if mode == "open_session_fail" => respond_err(
                out,
                &id,
                command,
                "open_session refused by fake-omp",
                "E_OPEN_REFUSED",
            ),
            "open_session" if mode == "hold_open" => self.hold(out, command),
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
            "prompt" if mode == "send_file" => {
                let calls = send_file_calls();
                self.start_tool_turn(&id, frame, calls, out);
            }
            "prompt" if mode == "tools" => {
                let (trigger_variable, default_trigger, calls_variable) = if self.lane {
                    (
                        "FAKE_OMP_LANE_TOOL_TRIGGER",
                        DEFAULT_LANE_TOOL_TRIGGER,
                        "FAKE_OMP_LANE_TOOL_CALLS",
                    )
                } else {
                    (
                        "FAKE_OMP_TOOL_TRIGGER",
                        DEFAULT_TOOL_TRIGGER,
                        "FAKE_OMP_TOOL_CALLS",
                    )
                };
                let trigger =
                    std::env::var(trigger_variable).unwrap_or_else(|_| default_trigger.to_owned());
                if prompt_message(frame).contains(&trigger) {
                    let calls = scripted_calls(calls_variable);
                    self.start_tool_turn(&id, frame, calls, out);
                } else {
                    prompt("ok", &id, frame, out);
                }
            }
            "prompt" if mode == "steer" => self.steer_prompt(&id, frame, out),
            "prompt" => prompt(mode, &id, frame, out),
            "host_tool_result" => {
                if frame.get("isError").and_then(Value::as_bool) == Some(true) {
                    self.tool_errors += 1;
                    touch_on_tool_error(self.tool_errors);
                }
                if let Some(active) = self.tool_turn.take() {
                    self.tool_turn = self.advance_tool_turn(active, out);
                }
            }
            _ => respond_err(out, &id, command, "unknown command", "E_UNKNOWN_COMMAND"),
        }
    }

    /// Writes the `fake_held` marker for `command`; nothing is written after it.
    fn hold(&mut self, out: &mut impl Write, command: &str) {
        send(out, &json!({"type": "fake_held", "command": command}));
        self.held = true;
    }

    fn start_tool_turn(
        &mut self,
        id: &Value,
        frame: &Value,
        calls: VecDeque<PlannedCall>,
        out: &mut impl Write,
    ) {
        respond_ok(out, id, "prompt", json!({"agentInvoked": true}));
        send(out, &json!({"type": "agent_start"}));
        compact_on_trigger(prompt_message(frame), out);
        let turn = ToolTurn {
            prompt_id: id.clone(),
            message: prompt_message(frame).to_owned(),
            remaining: calls,
            calls_made: 0,
        };
        self.tool_turn = self.advance_tool_turn(turn, out);
    }

    /// Emits the next host tool call, or finishes the turn when none is left.
    /// Returns the turn while it still waits for a `host_tool_result`.
    fn advance_tool_turn(&mut self, mut turn: ToolTurn, out: &mut impl Write) -> Option<ToolTurn> {
        let Some(call) = turn.remaining.pop_front() else {
            finish_echo(&turn.prompt_id, &turn.message, out);
            return None;
        };
        turn.calls_made += 1;
        self.total_calls += 1;
        let n = turn.calls_made;
        let tool_call_id = call.tool_call_id.unwrap_or_else(|| {
            if self.mode == "send_file" {
                format!("toolu_{n}")
            } else {
                format!("toolu_{}_{}", std::process::id(), self.total_calls)
            }
        });
        if let Some(path) = std::env::var_os("FAKE_OMP_LOG") {
            log_line(Path::new(&path), &format!("CALL host_{n}"));
        }
        send(
            out,
            &json!({
                "type": "host_tool_call",
                "id": format!("host_{n}"),
                "toolCallId": tool_call_id,
                "toolName": call.tool,
                "arguments": call.arguments,
            }),
        );
        Some(turn)
    }

    fn steer_prompt(&mut self, id: &Value, frame: &Value, out: &mut impl Write) {
        let is_steer = frame.get("streamingBehavior").and_then(Value::as_str) == Some("steer");
        if is_steer {
            if let Some(main) = self.steer_turn.take() {
                respond_ok(out, id, "prompt", json!({"agentInvoked": true}));
                self.answer_steer(&main, id, prompt_message(frame), out);
                return;
            }
        } else if !self.first_prompt_seen {
            self.first_prompt_seen = true;
            respond_ok(out, id, "prompt", json!({"agentInvoked": true}));
            send(out, &json!({"type": "agent_start"}));
            self.steer_turn = Some(SteerTurn {
                prompt_id: id.clone(),
                message: prompt_message(frame).to_owned(),
            });
            return;
        }
        prompt("ok", id, frame, out);
    }

    fn answer_steer(
        &mut self,
        main: &SteerTurn,
        steer_id: &Value,
        steer_message: &str,
        out: &mut impl Write,
    ) {
        let case = std::env::var("FAKE_OMP_STEER_CASE").unwrap_or_default();
        match case.as_str() {
            "merged" => {
                let text = format!("echo: {} + {steer_message}", main.message);
                end_run(&text, json!({}), out);
                completed(&main.prompt_id, out);
                completed(steer_id, out);
            }
            "separate" => {
                end_run(
                    &format!("echo: {}", main.message),
                    json!({"yielded": true, "isTerminal": false}),
                    out,
                );
                completed(&main.prompt_id, out);
                send(out, &json!({"type": "agent_start"}));
                end_run(&format!("echo: {steer_message}"), json!({}), out);
                completed(steer_id, out);
            }
            "hold" => self.hold(out, "prompt"),
            _ => {
                send(out, &json!({"type": "agent_end", "messages": []}));
                prompt_error(&main.prompt_id, out);
                prompt_error(steer_id, out);
            }
        }
    }
}

/// FAKE_OMP_SEND_FILE as `send_file` calls (unset or empty: one empty path).
fn send_file_calls() -> VecDeque<PlannedCall> {
    let script = std::env::var("FAKE_OMP_SEND_FILE").unwrap_or_default();
    let paths: Vec<String> = if script.is_empty() {
        vec![String::new()]
    } else {
        script.lines().map(str::to_owned).collect()
    };
    paths
        .into_iter()
        .map(|path| PlannedCall {
            tool: "send_file".to_owned(),
            arguments: json!({"path": path}),
            tool_call_id: None,
        })
        .collect()
}

/// Creates FAKE_OMP_TOUCH_ON_TOOL_ERROR when `errors`, the count of refused
/// calls so far, reaches FAKE_OMP_TOUCH_AFTER_ERRORS (default 1).
fn touch_on_tool_error(errors: usize) {
    let Some(path) = std::env::var_os("FAKE_OMP_TOUCH_ON_TOOL_ERROR") else {
        return;
    };
    let after = std::env::var("FAKE_OMP_TOUCH_AFTER_ERRORS")
        .map(|value| {
            value
                .parse::<usize>()
                .expect("FAKE_OMP_TOUCH_AFTER_ERRORS is a number")
        })
        .unwrap_or(1);
    if errors == after {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .expect("create FAKE_OMP_TOUCH_ON_TOOL_ERROR");
    }
}

/// The script in `variable`: one JSON object per non-empty line.
fn scripted_calls(variable: &str) -> VecDeque<PlannedCall> {
    let script = std::env::var(variable).unwrap_or_default();
    script
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let call: Value = serde_json::from_str(line).expect("tool script line is JSON");
            PlannedCall {
                tool: call
                    .get("tool")
                    .and_then(Value::as_str)
                    .expect("tool script line has a tool")
                    .to_owned(),
                arguments: call.get("arguments").cloned().unwrap_or_else(|| json!({})),
                tool_call_id: call.get("id").and_then(Value::as_str).map(str::to_owned),
            }
        })
        .collect()
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
            prompt_error(id, out);
        }
        _ => {
            compact_on_trigger(prompt_message(frame), out);
            finish_echo(id, prompt_message(frame), out);
        }
    }
}

/// Writes the compaction frame FAKE_OMP_COMPACT selects (default `auto`) when
/// `message` contains FAKE_OMP_COMPACT_TRIGGER.
fn compact_on_trigger(message: &str, out: &mut impl Write) {
    if !message_has_trigger(message, "FAKE_OMP_COMPACT_TRIGGER") {
        return;
    }
    let result = json!({
        "summary": "fake summary",
        "firstKeptEntryId": "fake-entry-1",
        "tokensBefore": 1000,
    });
    let kind = std::env::var("FAKE_OMP_COMPACT").unwrap_or_else(|_| "auto".to_owned());
    let frame = match kind.as_str() {
        "manual" => json!({"type": "manual_compaction_end", "aborted": false, "result": result}),
        "aborted" => json!({
            "type": "auto_compaction_end",
            "action": "context-full",
            "aborted": true,
            "willRetry": false,
        }),
        "skipped" => json!({
            "type": "auto_compaction_end",
            "action": "context-full",
            "aborted": false,
            "willRetry": false,
            "skipped": true,
        }),
        _ => json!({
            "type": "auto_compaction_end",
            "action": "context-full",
            "aborted": false,
            "willRetry": false,
            "result": result,
        }),
    };
    send(out, &frame);
}

/// `message` contains the text the environment variable `variable` holds
/// (an unset variable never matches).
fn message_has_trigger(message: &str, variable: &str) -> bool {
    std::env::var(variable).is_ok_and(|trigger| message.contains(&trigger))
}

/// The prompt's message without a leading `[moraebot ...]` envelope line.
fn prompt_message(frame: &Value) -> &str {
    let message = frame
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (first, rest) = message.split_once('\n').unwrap_or((message, ""));
    if first.starts_with("[moraebot ") && first.ends_with(']') {
        rest
    } else {
        message
    }
}

fn finish_echo(id: &Value, message: &str, out: &mut impl Write) {
    end_run(&format!("echo: {message}"), json!({}), out);
    let settled = !message_has_trigger(message, "FAKE_OMP_UNSETTLED_TRIGGER");
    completed_with(id, settled, out);
    if !settled && std::env::var("FAKE_OMP_SETTLE_FRAME").as_deref() == Ok("1") {
        send(out, &json!({"type": "session_settled"}));
    }
}

/// Writes `message_end` and `agent_end` for one assistant `text`; `extra`
/// fields are added to the `agent_end` frame.
fn end_run(text: &str, extra: Value, out: &mut impl Write) {
    let assistant = json!({
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "stopReason": "stop",
        "timestamp": 0,
    });
    send(out, &json!({"type": "message_end", "message": assistant}));
    let mut end = json!({"type": "agent_end", "messages": [assistant]});
    if let (Some(end), Value::Object(extra)) = (end.as_object_mut(), extra) {
        end.extend(extra);
    }
    send(out, &end);
}

fn completed(id: &Value, out: &mut impl Write) {
    completed_with(id, true, out);
}

fn completed_with(id: &Value, settled: bool, out: &mut impl Write) {
    send(
        out,
        &json!({
            "type": "prompt_result",
            "id": id,
            "agentInvoked": true,
            "status": "completed",
            "sessionSettled": settled,
        }),
    );
}

fn prompt_error(id: &Value, out: &mut impl Write) {
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

/// Appends `line` and its newline in one `write` call: a lead and its lanes share the
/// log, and `writeln!` on a `File` writes the text and the newline separately, so
/// another child's line could land between them and merge two lines into one.
fn log_line(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open FAKE_OMP_LOG");
    file.write_all(format!("{line}\n").as_bytes())
        .expect("write FAKE_OMP_LOG");
}
