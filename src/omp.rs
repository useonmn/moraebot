//! omp child manager: at most one live `omp --mode rpc` child in total.
//!
//! The child belongs to the conversation currently being served. Before another
//! conversation's child spawns, the previous one is closed and reaped, so two
//! processes never share a session folder. The official `omp_rpc::Client` is
//! blocking, so each child is owned by a dedicated OS thread; async callers talk
//! to it through channels. Message text reaches omp only as a JSON line on the
//! child's stdin, never as an argument or through a shell.

use std::ffi::OsString;
use std::fs::{DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use omp_rpc::{
    Client, ClientOptions, Error as RpcError, Event, GetSessionLeaseCommand, OpenSessionCommand,
    PromptCommand, PromptStatus, PromptTurn, RpcNotification,
};
use tokio::sync::oneshot;

use crate::config::Config;
use crate::store::Store;

/// The omp process could not be started or did not become ready.
pub const E_OMP_SPAWN: &str = "E_OMP_SPAWN";
/// `get_session_lease` failed before reopening a conversation.
pub const E_SESSION_LEASE: &str = "E_SESSION_LEASE";
/// Another process holds the conversation's omp session; no prompt was sent.
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

/// Reply text used when a completed turn produced no visible text.
pub const EMPTY_REPLY: &str = "(empty reply)";

const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Absolute path, so a `kill` shim earlier in PATH cannot intercept the group kill.
const KILL_BIN: &str = "/bin/kill";
/// How long shutdown waits for a killed child's worker thread to finish.
const ABORT_JOIN_BOUND: Duration = Duration::from_secs(10);
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

/// Exact omp argv (without the program) for one conversation. Non-empty `hints` become one
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

/// `<home>/conversations/<slug>/session`.
pub fn session_dir(config: &Config, key: &str) -> PathBuf {
    config
        .home
        .join("conversations")
        .join(key_slug(key))
        .join("session")
}

/// `<home>/logs/omp-<slug>.stderr.log`.
pub fn stderr_log_path(config: &Config, key: &str) -> PathBuf {
    config
        .home
        .join("logs")
        .join(format!("omp-{}.stderr.log", key_slug(key)))
}

/// Messenger names: a variable whose name contains one (any case) never reaches omp.
const MESSENGER_ENV_WORDS: [&str; 2] = ["DISCORD", "SLACK"];

/// Drops every variable whose name contains a messenger name; everything else (PATH,
/// HOME, model-provider credentials) is kept so omp can authenticate.
fn child_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    vars.filter(|(name, _)| {
        let name = name.to_string_lossy().to_ascii_uppercase();
        !MESSENGER_ENV_WORDS.iter().any(|word| name.contains(word))
    })
    .collect()
}

struct Started {
    pid: Option<u32>,
    session_id: String,
}

struct PromptRequest {
    text: String,
    timeout: Duration,
    done: oneshot::Sender<Result<PromptTurn, RpcError>>,
}

/// SIGKILLs the child's process group (the RPC client starts the child as its own
/// group leader). Best effort: the group may already be gone.
fn kill_group(pid: u32) {
    let _ = Command::new(KILL_BIN)
        .args(["-s", "KILL", "--"])
        .arg(format!("-{pid}"))
        .stderr(Stdio::null())
        .status();
}

/// Kills the child's group when the `Live` is dropped without being closed (a panic or a
/// cancelled caller), so a worker blocked in `prompt_and_wait` is released and the
/// client reaps the child. `close` and `abort` disarm it once the worker is joined,
/// because the pid may be reused after the child is reaped.
struct GroupKill(Option<u32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

/// The one live child and the thread that owns its client.
struct Live {
    key: String,
    pid: GroupKill,
    requests: mpsc::Sender<PromptRequest>,
    worker: JoinHandle<()>,
}

impl Live {
    /// Closes the client (on its own thread) and waits until the process is reaped.
    async fn close(self) {
        let Live {
            key,
            mut pid,
            requests,
            worker,
        } = self;
        drop(requests);
        match tokio::task::spawn_blocking(move || worker.join()).await {
            Ok(Ok(())) => pid.0 = None,
            _ => tracing::error!(conversation = %key, "omp worker thread panicked"),
        }
    }
}

impl Live {
    /// Kills the child's process group first so a worker blocked in `prompt_and_wait`
    /// sees the closed pipe and returns, then joins the worker within `ABORT_JOIN_BOUND`.
    async fn abort(self) {
        let Live {
            key,
            mut pid,
            requests,
            worker,
        } = self;
        if let Some(pid) = pid.0 {
            kill_group(pid);
        }
        drop(requests);
        let joined = tokio::task::spawn_blocking(move || worker.join());
        match tokio::time::timeout(ABORT_JOIN_BOUND, joined).await {
            Ok(Ok(Ok(()))) => pid.0 = None,
            Ok(_) => tracing::error!(conversation = %key, "omp worker thread panicked"),
            Err(_) => tracing::error!(conversation = %key, "omp worker did not stop in time"),
        }
    }
}

pub struct OmpManager {
    config: Config,
    store: Arc<Mutex<Store>>,
    turn_timeout: Duration,
    extra_env: Vec<(OsString, OsString)>,
    hints: String,
    live: tokio::sync::Mutex<Option<Live>>,
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
        Self {
            config,
            store,
            turn_timeout,
            extra_env: Vec::new(),
            hints: String::new(),
            live: Default::default(),
        }
    }

    /// Overrides the turn time limit (tests use a short one).
    pub fn with_turn_timeout(mut self, timeout: Duration) -> Self {
        self.turn_timeout = timeout;
        self
    }

    /// Extra variables for the omp child, subject to the same filter as the inherited ones.
    pub fn with_extra_env<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.extra_env
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Behavior hints for the messenger, appended to omp's system prompt; empty = none.
    pub fn with_hints(mut self, hints: String) -> Self {
        self.hints = hints;
        self
    }

    pub async fn child_pid(&self) -> Option<u32> {
        self.live.lock().await.as_ref().and_then(|live| live.pid.0)
    }

    /// Sends `text` to the conversation's omp session and waits for the turn.
    /// One prompt runs at a time across all conversations.
    pub async fn prompt(&self, key: &str, text: &str) -> Outcome {
        let mut live = self.live.lock().await;
        if live.as_ref().is_some_and(|l| l.key != key) {
            if let Some(previous) = live.take() {
                previous.close().await;
            }
        }
        if live.is_none() {
            match self.start(key).await {
                Ok(started) => *live = Some(started),
                Err(outcome) => return outcome,
            }
        }
        let Some(current) = live.as_ref() else {
            unreachable!("a child was started above")
        };

        let (done, result) = oneshot::channel();
        let request = PromptRequest {
            text: text.to_owned(),
            timeout: self.turn_timeout,
            done,
        };
        let turn = match current.requests.send(request) {
            Ok(()) => result.await.unwrap_or(Err(RpcError::Closed)),
            Err(_) => Err(RpcError::Closed),
        };
        let outcome = match turn {
            Ok(turn) => return map_turn(turn),
            Err(RpcError::Timeout { .. }) => {
                Outcome::needs_check(E_TURN_TIMEOUT, "the turn time limit expired")
            }
            Err(RpcError::Command { error, .. }) => {
                return Outcome::failed(E_PROMPT_REJECTED, error);
            }
            Err(RpcError::InvalidArgument(error)) => {
                return Outcome::failed(E_PROMPT_REJECTED, error);
            }
            // The turn ended with an error whose code is not `cost_cap`; the child is fine.
            Err(RpcError::Protocol(error)) if error.starts_with(PROMPT_RESULT_UNDECODABLE) => {
                return Outcome::failed(E_PROMPT_ERROR, error);
            }
            Err(error) => Outcome::needs_check(E_CHILD_LOST, error),
        };
        // Timeout or lost child: tear the child down before reporting.
        if let Some(current) = live.take() {
            tracing::warn!(conversation = %key, code = outcome.code(), "closing omp child");
            current.close().await;
        }
        outcome
    }

    /// Stops the omp child even in the middle of a turn; the turn's row is left for
    /// the caller to mark `needs_check`.
    pub async fn shutdown_all(&self) {
        if let Some(live) = self.live.lock().await.take() {
            live.abort().await;
        }
    }

    async fn start(&self, key: &str) -> Result<Live, Outcome> {
        let previous_session = self
            .store
            .lock()
            .expect("store mutex poisoned")
            .omp_session_id(key)
            .map_err(|e| Outcome::failed(e.code(), e))?;
        let session_dir = session_dir(&self.config, key);
        let command = self
            .command(key, &session_dir)
            .map_err(|e| Outcome::failed(E_OMP_SPAWN, e))?;
        let session_dir = session_dir.to_string_lossy().into_owned();

        let (requests, inbox) = mpsc::channel();
        let (started_tx, started) = oneshot::channel();
        let thread_key = key.to_owned();
        let worker = std::thread::Builder::new()
            .name(format!("omp-{}", key_slug(key)))
            .spawn(move || {
                run_child(
                    thread_key,
                    command,
                    session_dir,
                    previous_session,
                    started_tx,
                    inbox,
                )
            })
            .map_err(|e| Outcome::failed(E_OMP_SPAWN, e))?;

        let started = match started.await {
            Ok(Ok(started)) => started,
            Ok(Err(outcome)) => {
                let _ = tokio::task::spawn_blocking(move || worker.join()).await;
                return Err(outcome);
            }
            Err(_) => {
                let _ = tokio::task::spawn_blocking(move || worker.join()).await;
                return Err(Outcome::failed(
                    E_OMP_SPAWN,
                    "omp worker exited during startup",
                ));
            }
        };
        if let Err(e) = self
            .store
            .lock()
            .expect("store mutex poisoned")
            .set_omp_session_id(key, &started.session_id)
        {
            tracing::warn!(conversation = %key, code = e.code(), "could not store omp session id");
        }
        tracing::info!(conversation = %key, pid = started.pid, "omp child ready");
        Ok(Live {
            key: key.to_owned(),
            pid: GroupKill(started.pid),
            requests,
            worker,
        })
    }

    fn command(&self, key: &str, session_dir: &Path) -> std::io::Result<Command> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(session_dir)?;
        let stderr_path = stderr_log_path(&self.config, key);
        if let Some(logs) = stderr_path.parent() {
            DirBuilder::new().recursive(true).mode(0o700).create(logs)?;
        }
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&stderr_path)?;

        let mut command = Command::new(&self.config.omp_path);
        command
            .args(omp_args(&self.config, session_dir, &self.hints))
            .env_clear()
            .envs(child_env(
                std::env::vars_os().chain(self.extra_env.iter().cloned()),
            ))
            .stderr(Stdio::from(stderr));
        Ok(command)
    }
}

/// Body of the thread that owns one client: start, check the lease, open the
/// session, then serve prompts until the request channel closes.
fn run_child(
    key: String,
    command: Command,
    session_dir: String,
    previous_session: Option<String>,
    started: oneshot::Sender<Result<Started, Outcome>>,
    requests: mpsc::Receiver<PromptRequest>,
) {
    let options = ClientOptions {
        ready_timeout: READY_TIMEOUT,
        ..ClientOptions::default()
    };
    let (client, events) = match Client::spawn(command, options) {
        Ok(spawned) => spawned,
        Err(e) => {
            let _ = started.send(Err(Outcome::failed(E_OMP_SPAWN, e)));
            return;
        }
    };
    let pid = client.pid();
    let drain_key = key.clone();
    if let Err(e) = std::thread::Builder::new()
        .name(format!("omp-events-{}", key_slug(&key)))
        .spawn(move || drain_events(&drain_key, events))
    {
        close(&key, client);
        let _ = started.send(Err(Outcome::failed(E_OMP_SPAWN, e)));
        return;
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
            close(&key, client);
            let _ = started.send(Err(outcome));
            return;
        }
    }

    let opened = client.call(&OpenSessionCommand {
        session_dir,
        provider: None,
        model_id: None,
    });
    let session_id = match opened {
        Ok(opened) => opened.session_id,
        Err(e) => {
            close(&key, client);
            let _ = started.send(Err(Outcome::failed(E_OPEN_SESSION, e)));
            return;
        }
    };
    if started.send(Ok(Started { pid, session_id })).is_err() {
        close(&key, client);
        return;
    }

    for request in requests {
        let prompt = PromptCommand {
            message: request.text,
            images: None,
            streaming_behavior: None,
        };
        let _ = request
            .done
            .send(client.prompt_and_wait(&prompt, request.timeout));
    }
    close(&key, client);
}

fn close(key: &str, client: Client) {
    if let Err(e) = client.close() {
        tracing::warn!(conversation = %key, error = %e, "omp child teardown was not confirmed");
    }
}

/// The client's event channel is unbounded: drain it for the child's lifetime.
fn drain_events(key: &str, events: mpsc::Receiver<Event>) {
    for event in events {
        match event {
            Event::Notification(RpcNotification::ExtensionError(e)) => {
                tracing::warn!(conversation = %key, event = %e.event, "omp extension error");
            }
            Event::Undecodable { error, .. } => {
                tracing::warn!(conversation = %key, %error, "undecodable omp frame");
            }
            _ => {}
        }
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
}
