//! omp child manager: one lead `omp --mode rpc` child serves every conversation.
//!
//! The lead child is never closed because the next input belongs to another
//! conversation: each input names its own conversation (see `lead`), and the reply
//! stays with the row that asked. Its session folder is `<home>/lead/session` and its
//! omp session id is stored under `lead::LEAD_SESSION`. Lane children are owned by
//! `lane.rs` and built with the same plumbing (`child_command`, `open_child`).
//!
//! The official `omp_rpc::Client` is blocking, so the lead's client is owned by a
//! dedicated OS thread that serves each prompt on its own short-lived thread over a
//! shared client; async callers talk to it through channels. While a turn runs, one
//! newer message of the same conversation can be steered into it (`try_steer`).
//! Message text reaches omp only as a JSON line on the child's stdin, never as an
//! argument or through a shell.

use std::ffi::OsString;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use omp_rpc::{
    Client, ClientOptions, Error as RpcError, Event, GetSessionLeaseCommand, HostTool,
    HostToolError, OpenSessionCommand, PromptCommand, PromptStatus, PromptTurn, RpcAgentEvent,
    RpcNotification, StreamingBehavior, ToolLoadMode,
};
use serde_json::{Map, Value};
use tokio::sync::{Notify, oneshot, watch};

use crate::config::Config;
use crate::files;
use crate::lane::LaneManager;
use crate::lead;
use crate::memory::home::MemoryHome;
use crate::memory::{self, guard, history, render};
use crate::store::Store;

/// The omp process could not be started or did not become ready.
pub const E_OMP_SPAWN: &str = "E_OMP_SPAWN";
/// `get_session_lease` failed before reopening a session.
pub const E_SESSION_LEASE: &str = "E_SESSION_LEASE";
/// Another process holds the omp session; no prompt was sent.
pub const E_SESSION_BUSY: &str = "E_SESSION_BUSY";
/// `open_session` failed; no prompt was sent.
pub const E_OPEN_SESSION: &str = "E_OPEN_SESSION";
/// omp refused the prompt command itself.
pub const E_PROMPT_REJECTED: &str = "E_PROMPT_REJECTED";
/// The turn ended with `status: error`.
pub const E_PROMPT_ERROR: &str = "E_PROMPT_ERROR";
/// The turn ended with `status: aborted`.
pub const E_PROMPT_ABORTED: &str = "E_PROMPT_ABORTED";
/// The turn stopped at the configured `--max-cost` limit.
pub const E_COST_CAP: &str = "E_COST_CAP";
/// The configured turn time limit expired; the child was closed.
pub const E_TURN_TIMEOUT: &str = "E_TURN_TIMEOUT";
/// The child went away (or its transport broke) during the turn.
pub const E_CHILD_LOST: &str = "E_CHILD_LOST";

/// How the pinned `omp_rpc` client reports a `prompt_result` it could not decode. omp's
/// typed `error.code` accepts only `cost_cap`, so a turn that ended with any other error
/// code arrives as this protocol error instead of a `PromptTurn`.
const PROMPT_RESULT_UNDECODABLE: &str = "Failed to parse prompt_result";

/// Name of the host tool omp calls to hand a file back to the owner.
pub const SEND_FILE_TOOL: &str = "send_file";

/// Files the agent queued with `send_file` during the current turn (canonical paths).
type SentFiles = Arc<Mutex<Vec<PathBuf>>>;

/// Reply text used when a completed turn produced no visible text.
pub const EMPTY_REPLY: &str = "(empty reply)";

/// omp reads this variable as its session folder; it must never override `--session-dir`.
const SESSION_DIR_OVERRIDE: &str = "PI_CODING_AGENT_SESSION_DIR";

/// omp layers the config files named in this variable over its own config.
pub const CONFIG_FILES_ENV: &str = "PI_CONFIG_FILES";
/// File name of the omp config overlay in `<home>/omp`.
pub const OVERLAY_FILE: &str = "config-overlay.yml";
/// Overlay bytes: omp's own memory and autolearn are off for every moraebot child. `off`
/// is quoted so a YAML 1.1 reader cannot turn it into `false`.
pub const OMP_OVERLAY: &str = "memory:\n  backend: \"off\"\nautolearn:\n  enabled: false\n";

/// Counter for overlay temp file names, so concurrent spawns never share one.
static OVERLAY_TMP: AtomicU64 = AtomicU64::new(0);

const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Absolute path, so a `kill` shim earlier in PATH cannot intercept the group kill.
const KILL_BIN: &str = "/bin/kill";
/// How long shutdown waits for a killed child's worker thread to finish.
pub const ABORT_JOIN_BOUND: Duration = Duration::from_secs(10);
/// "No limit": `prompt_and_wait` needs a deadline, so use one that never matters.
const UNLIMITED: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Result of one prompt. The caller maps it to the inbox state and at most one notice.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The agent finished; the text to send back (never empty).
    Reply(String),
    /// omp handled the message without running the agent.
    NoAgent,
    /// The prompt did not run or ended in an error that is safe to report as failed.
    Failed { code: &'static str, detail: String },
    /// It is unknown whether the work ran (timeout, lost child) or the session is
    /// held elsewhere: mark the row `needs_check` and notify once.
    NeedsCheck { code: &'static str, detail: String },
}

impl Outcome {
    /// Stable code of a failed or needs-check outcome.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Failed { code, .. } | Self::NeedsCheck { code, .. } => Some(code),
            Self::Reply(_) | Self::NoAgent => None,
        }
    }

    fn failed(code: &'static str, detail: impl ToString) -> Self {
        Self::Failed {
            code,
            detail: detail.to_string(),
        }
    }

    fn needs_check(code: &'static str, detail: impl ToString) -> Self {
        Self::NeedsCheck {
            code,
            detail: detail.to_string(),
        }
    }
}

/// Result of one prompt plus the files the agent queued with `send_file` during it.
/// `files` is non-empty only when `outcome` is `Outcome::Reply`.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnReport {
    pub outcome: Outcome,
    pub files: Vec<PathBuf>,
}

impl TurnReport {
    fn without_files(outcome: Outcome) -> Self {
        Self {
            outcome,
            files: Vec::new(),
        }
    }
}

/// Result of one lead turn: the main row's report (with every file `send_file` queued
/// during the turn) and, when a row was steered into it, that row's id and outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupReport {
    pub main: TurnReport,
    pub steered: Option<(i64, Outcome)>,
}

/// The lead turn that is running: its main row and conversation. Host-tool handlers
/// read it through [`OmpManager::turn_context`].
#[derive(Debug, Clone, PartialEq)]
pub struct TurnContext {
    pub row_id: i64,
    pub conversation_key: String,
}

/// The `send_file` host tool. The handler (on the client's own thread) only checks the
/// path with `files::check_outbound`, refuses the file past `files::MAX_FILES` in this
/// turn, and queues the canonical path; dispatch uploads queued files after the reply.
fn send_file_tool(work_dir: PathBuf, home: PathBuf, sent: SentFiles) -> HostTool {
    let parameters: Map<String, Value> = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File to send: an absolute path, or a path relative to the work folder."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    }))
    .expect("send_file schema is a JSON object");
    let description = format!(
        "Send a file from the work folder to the owner. It is posted in this conversation \
         after your reply. `path` is absolute or relative to the work folder. At most {} files \
         per reply.",
        files::MAX_FILES
    );
    HostTool::new(
        SEND_FILE_TOOL,
        description,
        parameters,
        move |arguments, _| {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let refused =
                |code: &str, reason: &str| -> HostToolError { format!("{code}: {reason}").into() };
            let (canonical, _) = files::check_outbound(Path::new(path), &work_dir, &home)
                .map_err(|e| refused(e.code, &e.message))?;
            let mut sent = lock(&sent);
            if sent.len() >= files::MAX_FILES {
                return Err(refused(
                    files::E_FILE_TOO_MANY,
                    &format!("at most {} files per reply", files::MAX_FILES),
                ));
            }
            let name = canonical
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            sent.push(canonical);
            Ok(format!("Queued {name}; it is sent after your reply.").into())
        },
    )
    .load_mode(ToolLoadMode::Essential)
}

pub fn map_turn(turn: PromptTurn) -> Outcome {
    let Some(result) = turn.result else {
        return Outcome::NoAgent;
    };
    match result.status {
        PromptStatus::Completed => Outcome::Reply(
            turn.assistant_text
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| EMPTY_REPLY.to_owned()),
        ),
        PromptStatus::Aborted => Outcome::failed(E_PROMPT_ABORTED, "the turn was aborted"),
        PromptStatus::Error => match result.error {
            // The typed code decodes only the value `cost_cap`; other codes never get here
            // (see `PROMPT_RESULT_UNDECODABLE`).
            Some(error) if error.code.is_some() => Outcome::failed(E_COST_CAP, error.message),
            Some(error) => Outcome::failed(E_PROMPT_ERROR, error.message),
            None => Outcome::failed(E_PROMPT_ERROR, "the turn ended with an error"),
        },
    }
}

/// The one mapping of a prompt's result, for the lead's main and steer prompts and for
/// lanes. Only a timeout or a lost child is `NeedsCheck`; the caller closes the child then.
pub(crate) fn map_result(turn: Result<PromptTurn, RpcError>) -> Outcome {
    match turn {
        Ok(turn) => map_turn(turn),
        Err(RpcError::Timeout { .. }) => {
            Outcome::needs_check(E_TURN_TIMEOUT, "the turn time limit expired")
        }
        Err(RpcError::Command { error, .. }) => Outcome::failed(E_PROMPT_REJECTED, error),
        Err(RpcError::InvalidArgument(error)) => Outcome::failed(E_PROMPT_REJECTED, error),
        // The turn ended with an error whose code is not `cost_cap`; the child is fine.
        Err(RpcError::Protocol(error)) if error.starts_with(PROMPT_RESULT_UNDECODABLE) => {
            Outcome::failed(E_PROMPT_ERROR, error)
        }
        Err(error) => Outcome::needs_check(E_CHILD_LOST, error),
    }
}

/// Exact omp argv (without the program) for one child. Non-empty `hints` become one
/// `--append-system-prompt=<hints>` element; message text never goes into argv.
pub fn omp_args(config: &Config, session_dir: &Path, hints: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["--mode", "rpc", "--no-ui", "--session-dir"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(session_dir.into());
    args.push("--cwd".into());
    args.push(config.work_dir.clone().into());
    args.push("--approval-mode".into());
    args.push("yolo".into());
    if !hints.is_empty() {
        args.push(format!("--append-system-prompt={hints}").into());
    }
    if let Some(usd) = config.omp.max_cost_usd {
        args.push("--max-cost".into());
        args.push(usd.to_string().into());
    }
    args
}

/// Filesystem-safe form of a conversation key (`discord:th:1` -> `discord-th-1`).
pub fn key_slug(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `<home>/lead/session`.
pub fn lead_session_dir(config: &Config) -> PathBuf {
    config.home.join("lead").join("session")
}

/// `<home>/lanes/<id>/session`.
pub fn lane_session_dir(config: &Config, lane_id: i64) -> PathBuf {
    config
        .home
        .join("lanes")
        .join(lane_id.to_string())
        .join("session")
}

/// `<home>/logs/omp-<name>.stderr.log`; `name` is `lead::LEAD_SESSION` or `lane-<id>`.
pub fn stderr_log_path(config: &Config, name: &str) -> PathBuf {
    config
        .home
        .join("logs")
        .join(format!("omp-{}.stderr.log", key_slug(name)))
}

/// Messenger names: a variable whose name contains one (any case) never reaches omp.
const MESSENGER_ENV_WORDS: [&str; 2] = ["DISCORD", "SLACK"];

/// Drops every variable whose name contains a messenger name, and omp's session-folder
/// override; everything else (PATH, HOME, model-provider credentials) is kept so omp
/// can authenticate.
fn child_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    vars.filter(|(name, _)| {
        let name = name.to_string_lossy().to_ascii_uppercase();
        name != SESSION_DIR_OVERRIDE && !MESSENGER_ENV_WORDS.iter().any(|word| name.contains(word))
    })
    .collect()
}

/// The environment of an omp child or a Done check command: the process environment plus
/// `extra_env`, filtered by `child_env`.
pub(crate) fn check_env(extra_env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    child_env(std::env::vars_os().chain(extra_env.iter().cloned()))
}

/// Creates `path` and its parents, readable by the owner only.
pub(crate) fn private_dir(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// `<home>/omp/config-overlay.yml`.
pub fn overlay_path(config: &Config) -> PathBuf {
    config.home.join("omp").join(OVERLAY_FILE)
}

/// Makes sure the overlay file holds exactly `OMP_OVERLAY` (folder `0o700`, file `0o600`,
/// written to a temp file and renamed over the target) and returns its absolute path. omp
/// would resolve a relative path against the `--cwd` work dir, so the path is absolute even
/// when `config.home` is relative.
pub fn ensure_overlay(config: &Config) -> std::io::Result<PathBuf> {
    let path = overlay_path(config);
    let dir = config.home.join("omp");
    private_dir(&dir)?;
    if std::fs::read(&path).is_ok_and(|bytes| bytes == OMP_OVERLAY.as_bytes()) {
        return std::path::absolute(&path);
    }
    let n = OVERLAY_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".config-overlay.{}.{n}.tmp", std::process::id()));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(OMP_OVERLAY.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    std::path::absolute(&path)
}

/// The omp command for one child: `args`, stderr appended to `stderr_path` (mode
/// `0o600`), and the filtered environment plus `extra_env`. The lead and every lane use
/// it, so all children get exactly the same environment. `CONFIG_FILES_ENV` is set last
/// to the overlay from `ensure_overlay`, so it replaces any inherited or extra value and
/// omp's own memory stays off; an overlay that cannot be written fails the spawn.
pub(crate) fn child_command(
    config: &Config,
    args: Vec<OsString>,
    stderr_path: &Path,
    extra_env: &[(OsString, OsString)],
) -> std::io::Result<Command> {
    if let Some(logs) = stderr_path.parent() {
        private_dir(logs)?;
    }
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(stderr_path)?;
    let overlay = ensure_overlay(config)?;
    let mut command = Command::new(&config.omp_path);
    command
        .args(args)
        .env_clear()
        .envs(check_env(extra_env))
        .env(CONFIG_FILES_ENV, overlay)
        .stderr(Stdio::from(stderr));
    Ok(command)
}

/// Locks a std mutex, taking the data even when a holder panicked.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// SIGKILLs the child's process group (the RPC client starts the child as its own
/// group leader). Best effort: the group may already be gone.
pub(crate) fn kill_group(pid: u32) {
    let _ = Command::new(KILL_BIN)
        .args(["-s", "KILL", "--"])
        .arg(format!("-{pid}"))
        .stderr(Stdio::null())
        .status();
}

/// The process group of a spawned child. It kills the group when dropped while armed
/// (a panic or a cancelled caller), so a thread blocked on the child is released.
/// Whoever reaps the child disarms it, because the pid may be reused afterwards.
pub(crate) struct GroupKill(Option<u32>);

impl GroupKill {
    /// The group leader's pid while armed.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.0
    }

    /// Kills the group now; stays armed until the child is reaped.
    pub(crate) fn kill(&self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }

    /// The child was reaped: never signal its pid again.
    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A child that is ready for prompts: spawned, lease checked, session opened. Its event
/// drain thread runs until the client is closed.
pub(crate) struct OpenedChild {
    pub(crate) client: Client,
    pub(crate) pid: Option<u32>,
    pub(crate) session_id: String,
}

/// Spawns one omp child and opens its session, on the calling (blocking) thread.
///
/// `on_spawn` gets the child's process group right after `Client::spawn` returned and
/// before any command is sent; returning `false` closes (and reaps) the child and fails
/// with `E_OMP_SPAWN`. `on_event` sees every event before it is logged. With a
/// `previous_session`, the lease is checked first: held elsewhere is `E_SESSION_BUSY`.
pub(crate) fn open_child(
    name: &str,
    command: Command,
    tools: Vec<HostTool>,
    session_dir: String,
    previous_session: Option<String>,
    on_spawn: impl FnOnce(GroupKill) -> bool,
    on_event: impl Fn(&Event) + Send + 'static,
) -> Result<OpenedChild, Outcome> {
    let options = ClientOptions {
        ready_timeout: READY_TIMEOUT,
        tools,
        ..ClientOptions::default()
    };
    let (client, events) =
        Client::spawn(command, options).map_err(|e| Outcome::failed(E_OMP_SPAWN, e))?;
    let pid = client.pid();
    if !on_spawn(GroupKill(pid)) {
        close(name, client);
        return Err(Outcome::failed(
            E_OMP_SPAWN,
            "the child was refused at spawn",
        ));
    }
    let drain_name = name.to_owned();
    if let Err(e) = std::thread::Builder::new()
        .name(format!("omp-events-{}", key_slug(name)))
        .spawn(move || {
            for event in events {
                on_event(&event);
                log_event(&drain_name, &event);
            }
        })
    {
        close(name, client);
        return Err(Outcome::failed(E_OMP_SPAWN, e));
    }

    if let Some(session_id) = previous_session {
        let lease = client.call(&GetSessionLeaseCommand {
            session_ids: vec![session_id.clone()],
        });
        let refusal = match lease {
            Ok(lease) if lease.held.contains(&session_id) => Some(Outcome::needs_check(
                E_SESSION_BUSY,
                "another process holds this omp session",
            )),
            Ok(_) => None,
            Err(e) => Some(Outcome::failed(E_SESSION_LEASE, e)),
        };
        if let Some(outcome) = refusal {
            close(name, client);
            return Err(outcome);
        }
    }

    let opened = client.call(&OpenSessionCommand {
        session_dir,
        provider: None,
        model_id: None,
    });
    match opened {
        Ok(opened) => Ok(OpenedChild {
            client,
            pid,
            session_id: opened.session_id,
        }),
        Err(e) => {
            close(name, client);
            Err(Outcome::failed(E_OPEN_SESSION, e))
        }
    }
}

struct Started {
    pid: Option<u32>,
    session_id: String,
}

struct PromptRequest {
    text: String,
    timeout: Duration,
    behavior: StreamingBehavior,
    done: oneshot::Sender<Result<PromptTurn, RpcError>>,
}

/// The lead's process group, shared with `shutdown_all` and the turn's timeout path so
/// they can kill the child without waiting for the running turn.
type LeadKill = Arc<Mutex<Option<GroupKill>>>;

/// The child was reaped: drop its group without signalling it.
fn disarm(kill: &LeadKill) {
    if let Some(mut group) = lock(kill).take() {
        group.disarm();
    }
}

/// The steer sent into the open turn: its row and the receiver of its result.
struct Steer {
    row_id: i64,
    result: oneshot::Receiver<Result<PromptTurn, RpcError>>,
}

/// The lead turn that accepts a steer: set by `prompt` before the main frame is sent,
/// taken when the main prompt's wait returns (no steer joins afterwards).
struct OpenTurn {
    key: String,
    main_row_id: i64,
    /// The lead reported `agent_start` for this turn.
    started: bool,
    requests: mpsc::Sender<PromptRequest>,
    steer: Option<Steer>,
}

type SteerGate = Arc<Mutex<Option<OpenTurn>>>;

/// Closes the steer gate when dropped. A `prompt` cancelled mid-turn (the dispatcher drops
/// it at shutdown) would otherwise leave the gate holding a clone of the lead's request
/// sender, so the lead worker never sees its channel close and cannot be joined.
struct GateClose<'a>(&'a SteerGate);

impl Drop for GateClose<'_> {
    fn drop(&mut self) {
        lock(self.0).take();
    }
}

/// Opens the steer gate on the first `agent_start` of the open turn.
fn note_agent_start(gate: &SteerGate, running: &watch::Sender<Option<String>>, event: &Event) {
    if !matches!(
        event,
        Event::Notification(RpcNotification::RpcAgentEvent(RpcAgentEvent::AgentStart(_)))
    ) {
        return;
    }
    if let Some(turn) = lock(gate).as_mut()
        && !turn.started
    {
        turn.started = true;
        running.send_replace(Some(turn.key.clone()));
    }
}

/// The lead child and the thread that owns its client.
struct Live {
    requests: mpsc::Sender<PromptRequest>,
    worker: JoinHandle<()>,
    sent_files: SentFiles,
}

impl Live {
    /// Closes the client (on its own thread) and waits until the process is reaped.
    async fn close(self, kill: &LeadKill) {
        let Live {
            requests, worker, ..
        } = self;
        drop(requests);
        match tokio::task::spawn_blocking(move || worker.join()).await {
            Ok(Ok(())) => disarm(kill),
            _ => tracing::error!("omp lead worker thread panicked"),
        }
    }

    /// Kills the child's process group first so a prompt blocked on it sees the closed
    /// pipe and returns, then joins the worker within `ABORT_JOIN_BOUND`.
    async fn abort(self, kill: &LeadKill) {
        let Live {
            requests, worker, ..
        } = self;
        if let Some(group) = lock(kill).as_ref() {
            group.kill();
        }
        drop(requests);
        let joined = tokio::task::spawn_blocking(move || worker.join());
        match tokio::time::timeout(ABORT_JOIN_BOUND, joined).await {
            Ok(Ok(Ok(()))) => disarm(kill),
            Ok(_) => tracing::error!("omp lead worker thread panicked"),
            Err(_) => tracing::error!("omp lead worker did not stop in time"),
        }
    }
}

pub struct OmpManager {
    config: Config,
    store: Arc<Mutex<Store>>,
    turn_timeout: Duration,
    extra_env: Vec<(OsString, OsString)>,
    hints: String,
    host_tools: Vec<HostTool>,
    memory: Option<MemoryHome>,
    live: tokio::sync::Mutex<Option<Live>>,
    lead_kill: LeadKill,
    gate: SteerGate,
    running: Arc<watch::Sender<Option<String>>>,
    turn: Arc<Mutex<Option<TurnContext>>>,
    lanes: Arc<LaneManager>,
}

impl OmpManager {
    /// The turn limit comes from `omp.turn_timeout_minutes`; absent means no limit.
    pub fn new(config: Config, store: Arc<Mutex<Store>>) -> Self {
        let turn_timeout = config
            .omp
            .turn_timeout_minutes
            .map_or(UNLIMITED, |minutes| {
                Duration::from_secs(minutes.saturating_mul(60))
            });
        let turn: Arc<Mutex<Option<TurnContext>>> = Default::default();
        let lanes = Arc::new(LaneManager::new(
            config.clone(),
            Arc::clone(&store),
            Arc::clone(&turn),
            turn_timeout,
        ));
        Self {
            config,
            store,
            turn_timeout,
            extra_env: Vec::new(),
            hints: String::new(),
            host_tools: Vec::new(),
            memory: None,
            live: Default::default(),
            lead_kill: Default::default(),
            gate: Default::default(),
            running: Arc::new(watch::channel(None).0),
            turn,
            lanes,
        }
    }

    /// Overrides the turn time limit of the lead and every lane (tests use a short one).
    pub fn with_turn_timeout(mut self, timeout: Duration) -> Self {
        self.turn_timeout = timeout;
        // The lane manager is first shared when the lead child starts.
        Arc::get_mut(&mut self.lanes)
            .expect("lane manager not shared yet")
            .turn_timeout = timeout;
        self
    }

    /// Overrides the kill deadline of one lane Done check command (tests use a short one).
    pub fn with_check_timeout(mut self, timeout: Duration) -> Self {
        Arc::get_mut(&mut self.lanes)
            .expect("lane manager not shared yet")
            .check_timeout = timeout;
        self
    }

    /// Extra variables for the lead and every lane child, subject to the same filter as
    /// the inherited ones.
    pub fn with_extra_env<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let vars: Vec<(OsString, OsString)> = vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        // The lane manager is first shared when the lead child starts.
        Arc::get_mut(&mut self.lanes)
            .expect("lane manager not shared yet")
            .extra_env
            .extend(vars.iter().cloned());
        self.extra_env.extend(vars);
        self
    }

    /// System prompt text appended to the lead's (guidance and messenger hints); empty = none.
    pub fn with_hints(mut self, hints: String) -> Self {
        self.hints = hints;
        self
    }

    /// Host tools for the lead, registered after `send_file`.
    pub fn with_host_tools(mut self, tools: Vec<HostTool>) -> Self {
        self.host_tools.extend(tools);
        self
    }

    /// Owner memory for the lead: the block is rendered from `home` at each lead start and
    /// appended after the hints, and `memory_list`/`memory_read` follow the host tools.
    pub fn with_memory(mut self, home: MemoryHome) -> Self {
        self.memory = Some(home);
        self
    }

    /// The running turn's main row and conversation, for host-tool handlers; `None`
    /// between turns.
    pub fn turn_context(&self) -> Arc<Mutex<Option<TurnContext>>> {
        Arc::clone(&self.turn)
    }

    /// `Some(conversation key)` from the lead's first `agent_start` of a turn until the
    /// turn's group ends; a same-conversation message can be steered in meanwhile.
    pub fn running_turn(&self) -> watch::Receiver<Option<String>> {
        self.running.subscribe()
    }

    /// Notified after a lane result row was stored; the dispatcher then looks for work.
    pub fn lane_wake(&self) -> Arc<Notify> {
        self.lanes.wake()
    }

    pub fn lanes(&self) -> &LaneManager {
        &self.lanes
    }

    pub async fn child_pid(&self) -> Option<u32> {
        let live = self.live.lock().await;
        live.as_ref()?;
        lock(&self.lead_kill).as_ref().and_then(|group| group.0)
    }

    /// Sends `text` (row `row_id` of conversation `key`) to the lead and waits for the
    /// turn, including a message steered into it, then reports the group with the files
    /// queued by `send_file` during it. One lead turn runs at a time. The child is closed
    /// only when an outcome is `NeedsCheck` (timeout or lost child).
    pub async fn prompt(&self, key: &str, row_id: i64, text: &str) -> GroupReport {
        let mut live = self.live.lock().await;
        if live.is_none() {
            match self.start().await {
                Ok(started) => *live = Some(started),
                Err(outcome) => {
                    return GroupReport {
                        main: TurnReport::without_files(outcome),
                        steered: None,
                    };
                }
            }
        }
        let Some(current) = live.as_ref() else {
            unreachable!("a child was started above")
        };

        // The live mutex is held for the whole turn, so the bucket belongs to this turn only.
        take_sent(&current.sent_files);
        *lock(&self.turn) = Some(TurnContext {
            row_id,
            conversation_key: key.to_owned(),
        });
        let (done, result) = oneshot::channel();
        let request = PromptRequest {
            text: text.to_owned(),
            timeout: self.turn_timeout,
            behavior: StreamingBehavior::FollowUp,
            done,
        };
        let _gate_close = GateClose(&self.gate);
        let sent = {
            let mut gate = lock(&self.gate);
            *gate = Some(OpenTurn {
                key: key.to_owned(),
                main_row_id: row_id,
                started: false,
                requests: current.requests.clone(),
                steer: None,
            });
            current.requests.send(request).is_ok()
        };
        let main = if sent {
            result.await.unwrap_or(Err(RpcError::Closed))
        } else {
            Err(RpcError::Closed)
        };
        // Close the gate: no steer joins this turn from here on.
        let steer = lock(&self.gate).take().and_then(|turn| turn.steer);
        let main_timed_out = matches!(main, Err(RpcError::Timeout { .. }));
        let main = map_result(main);

        let mut killed = false;
        let steered = match steer {
            None => None,
            Some(Steer {
                row_id: steer_row,
                mut result,
            }) => {
                let outcome = if main_timed_out {
                    // Never let the child outlive the turn limit by waiting for the steer.
                    self.kill_lead();
                    killed = true;
                    map_result(result.await.unwrap_or(Err(RpcError::Closed)))
                } else {
                    match tokio::time::timeout(self.turn_timeout, &mut result).await {
                        Ok(turn) => map_result(turn.unwrap_or(Err(RpcError::Closed))),
                        Err(_) => {
                            self.kill_lead();
                            killed = true;
                            let _ = result.await;
                            Outcome::needs_check(E_TURN_TIMEOUT, "the turn time limit expired")
                        }
                    }
                };
                Some((steer_row, outcome))
            }
        };
        let files = take_sent(&current.sent_files);

        let lost = killed
            || matches!(main, Outcome::NeedsCheck { .. })
            || matches!(steered, Some((_, Outcome::NeedsCheck { .. })));
        if lost
            && let Some(current) = live.take()
        {
            tracing::warn!(
                conversation = %key,
                row = row_id,
                code = main.code(),
                "closing omp lead child"
            );
            current.close(&self.lead_kill).await;
        }
        *lock(&self.turn) = None;
        self.running.send_replace(None);
        GroupReport {
            main: report(key, main, files),
            steered,
        }
    }

    /// Sends `text` (row `row_id`) into the running lead turn as a steer. Only when that
    /// turn is for `key`, the lead reported `agent_start` for it and it holds no steer
    /// yet; then `claim` gets the turn's main row id and must claim the row in the store
    /// (false: nothing is sent). Returns whether the row was steered.
    pub fn try_steer(
        &self,
        key: &str,
        row_id: i64,
        text: &str,
        claim: impl FnOnce(i64) -> bool,
    ) -> bool {
        let mut gate = lock(&self.gate);
        let Some(turn) = gate.as_mut() else {
            return false;
        };
        if turn.key != key || !turn.started || turn.steer.is_some() {
            return false;
        }
        if !claim(turn.main_row_id) {
            return false;
        }
        let (done, result) = oneshot::channel();
        let request = PromptRequest {
            text: text.to_owned(),
            // A backstop only: `prompt` always ends the steer wait first.
            timeout: self.turn_timeout.saturating_mul(2),
            behavior: StreamingBehavior::Steer,
            done,
        };
        // A failed send drops `done`, so the claimed row is reported as a lost child.
        let _ = turn.requests.send(request);
        turn.steer = Some(Steer { row_id, result });
        tracing::info!(conversation = %key, row = row_id, main = turn.main_row_id, "steered into the running turn");
        true
    }

    /// Stops the lead child even in the middle of a turn, without waiting for the turn:
    /// the kill makes a running `prompt` report `NeedsCheck(E_CHILD_LOST)` and release
    /// the child, and whatever is left is aborted. Lanes stop accepting first and every
    /// lane child is killed too. Rows and lanes are left for the caller to mark.
    pub async fn shutdown_all(&self) {
        self.lanes.stop_accepting();
        self.kill_lead();
        self.lanes.shutdown_all().await;
        if let Some(live) = self.live.lock().await.take() {
            live.abort(&self.lead_kill).await;
        }
    }

    fn kill_lead(&self) {
        if let Some(group) = lock(&self.lead_kill).as_ref() {
            group.kill();
        }
    }

    async fn start(&self) -> Result<Live, Outcome> {
        let previous_session = lock(&self.store)
            .session_id(lead::LEAD_SESSION)
            .map_err(|e| Outcome::failed(e.code(), e))?;
        let session_dir = lead_session_dir(&self.config);
        let append = match &self.memory {
            Some(home) => {
                let rendered = {
                    let mut store = lock(&self.store);
                    match history::prepare(&mut store, home).and_then(|committed| {
                        history::dirty(home, &committed)
                            .map(|dirty| render::render(&committed, &dirty, guard::unix_now()))
                    }) {
                        Ok(rendered) => rendered,
                        Err(e) => {
                            tracing::warn!(code = e.code, "memory history unavailable");
                            render::unavailable(e.code)
                        }
                    }
                };
                for (path, code) in &rendered.problems {
                    tracing::warn!(path = %path, code, "memory file not rendered");
                }
                render::append(&self.hints, &rendered.text)
            }
            None => self.hints.clone(),
        };
        let command = private_dir(&session_dir)
            .and_then(|()| {
                child_command(
                    &self.config,
                    omp_args(&self.config, &session_dir, &append),
                    &stderr_log_path(&self.config, lead::LEAD_SESSION),
                    &self.extra_env,
                )
            })
            .map_err(|e| Outcome::failed(E_OMP_SPAWN, e))?;
        let session_dir = session_dir.to_string_lossy().into_owned();

        let (requests, inbox) = mpsc::channel();
        let (started_tx, started) = oneshot::channel();
        let sent_files = SentFiles::default();
        let mut tools = vec![send_file_tool(
            self.config.work_dir.clone(),
            self.config.home.clone(),
            Arc::clone(&sent_files),
        )];
        tools.extend(self.host_tools.iter().cloned());
        if let Some(home) = &self.memory {
            tools.extend(memory::tools::host_tools(
                home.clone(),
                Arc::clone(&self.store),
                Arc::clone(&self.turn),
            ));
        }
        tools.extend(self.lanes.host_tools());
        let lead_kill = Arc::clone(&self.lead_kill);
        let gate = Arc::clone(&self.gate);
        let running = Arc::clone(&self.running);
        let worker = std::thread::Builder::new()
            .name("omp-lead".to_owned())
            .spawn(move || {
                let opened = open_child(
                    lead::LEAD_SESSION,
                    command,
                    tools,
                    session_dir,
                    previous_session,
                    move |group| {
                        *lock(&lead_kill) = Some(group);
                        true
                    },
                    move |event| note_agent_start(&gate, &running, event),
                );
                match opened {
                    Err(outcome) => {
                        let _ = started_tx.send(Err(outcome));
                    }
                    Ok(OpenedChild {
                        client,
                        pid,
                        session_id,
                    }) => {
                        if started_tx.send(Ok(Started { pid, session_id })).is_err() {
                            close(lead::LEAD_SESSION, client);
                            return;
                        }
                        serve_lead(client, inbox);
                    }
                }
            })
            .map_err(|e| Outcome::failed(E_OMP_SPAWN, e))?;

        let started = match started.await {
            Ok(Ok(started)) => started,
            failed => {
                let _ = tokio::task::spawn_blocking(move || worker.join()).await;
                // `open_child` reaped the child (if one was spawned).
                disarm(&self.lead_kill);
                return Err(match failed {
                    Ok(Err(outcome)) => outcome,
                    _ => Outcome::failed(E_OMP_SPAWN, "omp worker exited during startup"),
                });
            }
        };
        if let Err(e) = lock(&self.store).set_session_id(lead::LEAD_SESSION, &started.session_id) {
            tracing::warn!(code = e.code(), "could not store the lead's omp session id");
        }
        tracing::info!(pid = started.pid, "omp lead child ready");
        Ok(Live {
            requests,
            worker,
            sent_files,
        })
    }
}

/// Serves the lead's prompts until the request channel closes. Each prompt waits on its
/// own thread (concurrent waits on one client are allowed), so a steer can be sent
/// while the main prompt waits. The client is closed after every wait has joined.
fn serve_lead(client: Client, requests: mpsc::Receiver<PromptRequest>) {
    let client = Arc::new(client);
    let mut waits: Vec<JoinHandle<()>> = Vec::new();
    for request in requests {
        let (finished, open): (Vec<_>, Vec<_>) =
            waits.into_iter().partition(JoinHandle::is_finished);
        for wait in finished {
            let _ = wait.join();
        }
        waits = open;
        let PromptRequest {
            text,
            timeout,
            behavior,
            done,
        } = request;
        let client = Arc::clone(&client);
        // A failed spawn drops `done`, which the caller reads as a lost child.
        match std::thread::Builder::new()
            .name("omp-lead-prompt".to_owned())
            .spawn(move || {
                let prompt = PromptCommand {
                    message: text,
                    images: None,
                    streaming_behavior: Some(behavior),
                };
                let _ = done.send(client.prompt_and_wait(&prompt, timeout));
            }) {
            Ok(wait) => waits.push(wait),
            Err(e) => tracing::warn!(error = %e, "could not start an omp prompt thread"),
        }
    }
    for wait in waits {
        let _ = wait.join();
    }
    match Arc::try_unwrap(client) {
        Ok(client) => close(lead::LEAD_SESSION, client),
        // Dropping the last handle runs the same teardown.
        Err(_) => tracing::error!("omp lead client still shared at close"),
    }
}

/// Empties the turn's bucket and returns what it held.
fn take_sent(sent: &SentFiles) -> Vec<PathBuf> {
    std::mem::take(&mut *lock(sent))
}

/// Keeps the queued files only for a reply; otherwise they are dropped and only their
/// count is logged.
fn report(key: &str, outcome: Outcome, files: Vec<PathBuf>) -> TurnReport {
    if matches!(outcome, Outcome::Reply(_)) || files.is_empty() {
        return TurnReport { outcome, files };
    }
    tracing::warn!(
        conversation = %key,
        code = outcome.code(),
        dropped = files.len(),
        "turn did not reply; queued files dropped"
    );
    TurnReport::without_files(outcome)
}

pub(crate) fn close(name: &str, client: Client) {
    if let Err(e) = client.close() {
        tracing::warn!(child = %name, error = %e, "omp child teardown was not confirmed");
    }
}

/// Logs the events worth a line; the client's event channel is drained for the
/// child's lifetime.
fn log_event(name: &str, event: &Event) {
    match event {
        Event::Notification(RpcNotification::ExtensionError(e)) => {
            tracing::warn!(child = %name, event = %e.event, "omp extension error");
        }
        Event::Undecodable { error, .. } => {
            tracing::warn!(child = %name, %error, "undecodable omp frame");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omp_rpc::{PromptError, PromptResultEvent};

    fn turn(
        status: Option<PromptStatus>,
        text: Option<&str>,
        error: Option<PromptError>,
    ) -> PromptTurn {
        PromptTurn {
            events: Vec::new(),
            messages: Vec::new(),
            assistant_message: None,
            assistant_text: text.map(str::to_owned),
            result: status.map(|status| PromptResultEvent {
                agent_invoked: true,
                status,
                session_settled: true,
                id: None,
                error,
            }),
        }
    }

    fn prompt_error(code: Option<&str>) -> PromptError {
        serde_json::from_value(serde_json::json!({
            "message": "stopped",
            "retryable": false,
            "code": code,
        }))
        .expect("valid prompt error")
    }

    #[test]
    fn completed_turn_replies_with_text() {
        let outcome = map_turn(turn(Some(PromptStatus::Completed), Some("done"), None));
        assert_eq!(outcome, Outcome::Reply("done".into()));
    }

    #[test]
    fn completed_turn_without_text_uses_empty_reply() {
        for text in [None, Some(""), Some("  \n")] {
            let outcome = map_turn(turn(Some(PromptStatus::Completed), text, None));
            assert_eq!(outcome, Outcome::Reply(EMPTY_REPLY.into()));
        }
    }

    #[test]
    fn missing_result_is_no_agent() {
        assert_eq!(map_turn(turn(None, None, None)), Outcome::NoAgent);
    }

    #[test]
    fn error_statuses_map_to_codes() {
        let aborted = map_turn(turn(Some(PromptStatus::Aborted), None, None));
        assert_eq!(aborted.code(), Some(E_PROMPT_ABORTED));
        let error = map_turn(turn(
            Some(PromptStatus::Error),
            None,
            Some(prompt_error(None)),
        ));
        assert_eq!(error.code(), Some(E_PROMPT_ERROR));
        let cap = map_turn(turn(
            Some(PromptStatus::Error),
            None,
            Some(prompt_error(Some("cost_cap"))),
        ));
        assert_eq!(cap.code(), Some(E_COST_CAP));
        assert!(matches!(cap, Outcome::Failed { .. }));
    }

    #[test]
    fn only_cost_cap_decodes_as_a_typed_error_code() {
        let other = serde_json::from_value::<PromptError>(serde_json::json!({
            "message": "stopped",
            "retryable": false,
            "code": "provider_error",
        }));
        assert!(
            other.is_err(),
            "other codes reach the client as a protocol error"
        );
    }

    #[test]
    fn map_result_maps_errors() {
        assert_eq!(map_result(Err(RpcError::Closed)).code(), Some(E_CHILD_LOST));
        assert!(matches!(
            map_result(Err(RpcError::Closed)),
            Outcome::NeedsCheck { .. }
        ));
        let rejected = map_result(Err(RpcError::InvalidArgument("bad".into())));
        assert!(matches!(rejected, Outcome::Failed { .. }));
        assert_eq!(rejected.code(), Some(E_PROMPT_REJECTED));
        let undecodable = map_result(Err(RpcError::Protocol(format!(
            "{PROMPT_RESULT_UNDECODABLE}: unknown code"
        ))));
        assert!(matches!(undecodable, Outcome::Failed { .. }));
        assert_eq!(undecodable.code(), Some(E_PROMPT_ERROR));
        let ok = turn(Some(PromptStatus::Completed), Some("done"), None);
        assert_eq!(map_result(Ok(ok.clone())), map_turn(ok));
    }

    #[test]
    fn key_slug_replaces_separators() {
        assert_eq!(key_slug("discord:th:111"), "discord-th-111");
        assert_eq!(key_slug("a/../b"), "a----b");
    }

    #[test]
    fn child_env_drops_only_messenger_names() {
        let vars = [
            ("PATH", "p"),
            ("HOME", "h"),
            ("MY_DISCORD_X", "1"),
            ("discord_y", "2"),
            ("SLACK_X", "3"),
            ("my_slack_y", "4"),
            ("PROVIDER_API_KEY", "k"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let names: Vec<String> = child_env(vars)
            .into_iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "HOME", "PROVIDER_API_KEY"]);
    }

    #[test]
    fn check_env_drops_messenger_names() {
        let names: Vec<String> =
            check_env(&[("SLACK_X".into(), "1".into()), ("OTHER".into(), "1".into())])
                .into_iter()
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect();
        assert!(names.iter().any(|name| name == "OTHER"));
        assert!(!names.iter().any(|name| {
            let name = name.to_ascii_uppercase();
            name.contains("SLACK") || name.contains("DISCORD")
        }));
    }

    #[test]
    fn child_env_drops_session_dir_override() {
        let vars = [
            ("PATH", "p"),
            (SESSION_DIR_OVERRIDE, "/elsewhere"),
            ("PI_CODING_AGENT_DIR", "d"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let names: Vec<String> = child_env(vars)
            .into_iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "PI_CODING_AGENT_DIR"]);
    }
}
