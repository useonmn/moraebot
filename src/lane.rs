//! Lanes: separate headless omp children started by the lead through host tools. Messenger-agnostic.
//!
//! The lead calls `lane_start`, `lane_message` and `lane_status` (registered on the lead
//! child next to `send_file`). A lane is its own omp RPC child with no host tools and no
//! appended system prompt; it runs one prompt on a worker thread and closes. Its result
//! is stored as one queued inbox row (`platform = LANE_PLATFORM`) in the same
//! transaction as the lane's new state, and the dispatcher is woken; the lead answers it
//! in a normal turn. Handlers never wait for lane work.
//!
//! Lock order is `inner` -> store everywhere. A handler holds `inner` from the
//! `stopping` check until the new worker is registered, and a worker writes its result
//! and removes its own registration under one hold of `inner`, so at most one worker per
//! lane is ever registered and `shutdown_all` sees every lane child that exists.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use omp_rpc::{HostTool, HostToolError, PromptCommand, ToolLoadMode};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};

use crate::config::Config;
use crate::lead::{self, LaneResult};
use crate::omp::{
    self, ABORT_JOIN_BOUND, E_OMP_SPAWN, EMPTY_REPLY, GroupKill, OpenedChild, Outcome, TurnContext,
    lock,
};
use crate::store::{LaneMessage, LaneStart, LaneState, Store};

/// Most lanes that may run a turn at the same time.
pub const MAX_RUNNING_LANES: usize = 3;
/// Inbox `platform` of rows that carry a lane's result to the lead.
pub const LANE_PLATFORM: &str = "lane";
/// Host tool that starts a new lane.
pub const LANE_START_TOOL: &str = "lane_start";
/// Host tool that sends a follow-up to an idle lane.
pub const LANE_MESSAGE_TOOL: &str = "lane_message";
/// Host tool that lists lanes and their state.
pub const LANE_STATUS_TOOL: &str = "lane_status";

/// A lane tool call had missing or invalid arguments.
pub const E_LANE_ARGS: &str = "E_LANE_ARGS";
/// No lane has the given id.
pub const E_LANE_UNKNOWN: &str = "E_LANE_UNKNOWN";
/// The lane is still running a turn.
pub const E_LANE_BUSY: &str = "E_LANE_BUSY";
/// `MAX_RUNNING_LANES` lanes are already running.
pub const E_LANE_LIMIT: &str = "E_LANE_LIMIT";
/// The lane was running when moraebot stopped or restarted.
pub const E_LANE_INTERRUPTED: &str = "E_LANE_INTERRUPTED";
/// moraebot is shutting down and starts no new lane.
pub const E_LANE_STOPPING: &str = "E_LANE_STOPPING";

/// `lane_status` cuts each task to this many characters.
const STATUS_TASK_CHARS: usize = 200;

/// One registered lane worker: its child's process group once spawned, and its thread.
struct LaneChild {
    kill: Option<GroupKill>,
    worker: JoinHandle<()>,
}

#[derive(Default)]
struct LaneSet {
    /// Set by `stop_accepting`: no lane starts and no child is kept from then on.
    stopping: bool,
    children: HashMap<i64, LaneChild>,
}

/// One lane prompt for a worker thread.
struct Job {
    id: i64,
    seq: i64,
    conversation_key: String,
    text: String,
    previous_session: Option<String>,
}

pub struct LaneManager {
    config: Config,
    store: Arc<Mutex<Store>>,
    wake: Arc<Notify>,
    turn: Arc<Mutex<Option<TurnContext>>>,
    turn_timeout: Duration,
    inner: Mutex<LaneSet>,
    pids: watch::Sender<Vec<u32>>,
    pub(crate) extra_env: Vec<(OsString, OsString)>,
}

fn refused(code: &str, reason: &str) -> HostToolError {
    format!("{code}: {reason}").into()
}

fn schema(value: Value) -> Map<String, Value> {
    serde_json::from_value(value).expect("lane tool schema is a JSON object")
}

impl LaneManager {
    /// `turn` is the lead's turn context; `turn_timeout` the configured turn limit.
    pub(crate) fn new(
        config: Config,
        store: Arc<Mutex<Store>>,
        turn: Arc<Mutex<Option<TurnContext>>>,
        turn_timeout: Duration,
    ) -> Self {
        Self {
            config,
            store,
            wake: Arc::default(),
            turn,
            turn_timeout,
            inner: Mutex::default(),
            pids: watch::channel(Vec::new()).0,
            extra_env: Vec::new(),
        }
    }

    /// Notified after a lane result row was stored.
    pub(crate) fn wake(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    /// Pids of lane children that are spawned and not yet reaped.
    pub fn running_pids(&self) -> watch::Receiver<Vec<u32>> {
        self.pids.subscribe()
    }

    /// From now on `lane_start` and `lane_message` refuse with `E_LANE_STOPPING`.
    pub fn stop_accepting(&self) {
        lock(&self.inner).stopping = true;
    }

    /// Stops accepting, then kills every lane child (including one still starting) and
    /// joins its worker within `ABORT_JOIN_BOUND`. A lane the kill ended keeps its
    /// `running` row for `recover_on_start`.
    pub async fn shutdown_all(&self) {
        let children: Vec<(i64, LaneChild)> = {
            let mut set = lock(&self.inner);
            set.stopping = true;
            set.children.drain().collect()
        };
        for (_, child) in &children {
            if let Some(group) = &child.kill {
                group.kill();
            }
        }
        for (id, LaneChild { kill, worker }) in children {
            let joined = tokio::task::spawn_blocking(move || worker.join());
            match tokio::time::timeout(ABORT_JOIN_BOUND, joined).await {
                Ok(Ok(Ok(()))) => {
                    // The worker reaped its child: never signal that pid again.
                    if let Some(mut group) = kill {
                        group.disarm();
                    }
                }
                Ok(_) => tracing::error!(lane = id, "lane worker thread panicked"),
                Err(_) => tracing::error!(lane = id, "lane worker did not stop in time"),
            }
        }
    }

    /// The three lane tools for the lead child.
    pub fn host_tools(self: &Arc<Self>) -> Vec<HostTool> {
        let start = Arc::clone(self);
        let message = Arc::clone(self);
        let status = Arc::clone(self);
        vec![
            HostTool::new(
                LANE_START_TOOL,
                "Start a lane: a separate agent that works on `task` in the background, in \
                 the same work folder. `task` must be self-contained; the lane does not see \
                 the conversation. Returns the lane id at once; the lane's result comes back \
                 to you later as a new message.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "The self-contained task."}
                    },
                    "required": ["task"],
                    "additionalProperties": false
                })),
                move |arguments, ctx| start.lane_start(&arguments, ctx.tool_call_id()),
            )
            .load_mode(ToolLoadMode::Essential),
            HostTool::new(
                LANE_MESSAGE_TOOL,
                "Send more work to a lane whose result already arrived. A running lane \
                 refuses. The result comes back to you later as a new message.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "lane_id": {"type": "integer"},
                        "text": {"type": "string"}
                    },
                    "required": ["lane_id", "text"],
                    "additionalProperties": false
                })),
                move |arguments, _| message.lane_message(&arguments),
            )
            .load_mode(ToolLoadMode::Essential),
            HostTool::new(
                LANE_STATUS_TOOL,
                "List lanes with their state, or one lane when `lane_id` is given.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "lane_id": {"type": "integer"}
                    },
                    "additionalProperties": false
                })),
                move |arguments, _| status.lane_status(&arguments),
            )
            .load_mode(ToolLoadMode::Essential),
        ]
    }

    fn lane_start(
        self: &Arc<Self>,
        arguments: &Map<String, Value>,
        tool_call_id: &str,
    ) -> Result<omp_rpc::HostToolOutput, HostToolError> {
        let task = arguments
            .get("task")
            .and_then(Value::as_str)
            .filter(|task| !task.trim().is_empty())
            .ok_or_else(|| refused(E_LANE_ARGS, "task must be a non-empty string"))?;
        let conversation_key = lock(&self.turn)
            .as_ref()
            .map(|turn| turn.conversation_key.clone())
            .ok_or_else(|| refused(E_LANE_ARGS, "no active turn"))?;
        let mut set = lock(&self.inner);
        if set.stopping {
            return Err(refused(E_LANE_STOPPING, "the gateway is stopping"));
        }
        let started = lock(&self.store)
            .start_lane(tool_call_id, &conversation_key, task, MAX_RUNNING_LANES)
            .map_err(|e| refused(e.code(), &e.to_string()))?;
        let id = match started {
            LaneStart::New(id) => {
                self.spawn_worker(
                    &mut set,
                    Job {
                        id,
                        seq: 1,
                        conversation_key,
                        text: task.to_owned(),
                        previous_session: None,
                    },
                );
                id
            }
            LaneStart::Existing(id) => id,
            LaneStart::Limit => {
                return Err(refused(
                    E_LANE_LIMIT,
                    &format!("{MAX_RUNNING_LANES} lanes are already running"),
                ));
            }
        };
        Ok(format!(
            "{}\nThe lane's result comes back to you as a new message.",
            json!({ "lane_id": id })
        )
        .into())
    }

    fn lane_message(
        self: &Arc<Self>,
        arguments: &Map<String, Value>,
    ) -> Result<omp_rpc::HostToolOutput, HostToolError> {
        let id = arguments
            .get("lane_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| refused(E_LANE_ARGS, "lane_id must be an integer"))?;
        let text = arguments
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| refused(E_LANE_ARGS, "text must be a non-empty string"))?;
        let mut set = lock(&self.inner);
        if set.stopping {
            return Err(refused(E_LANE_STOPPING, "the gateway is stopping"));
        }
        let job = {
            let mut store = lock(&self.store);
            let store_err = |e: crate::store::StoreError| refused(e.code(), &e.to_string());
            match store
                .message_lane(id, MAX_RUNNING_LANES)
                .map_err(store_err)?
            {
                LaneMessage::Seq(seq) => {
                    let lane = store
                        .lane(id)
                        .map_err(store_err)?
                        .ok_or_else(|| refused(E_LANE_UNKNOWN, "no lane has this id"))?;
                    Job {
                        id,
                        seq,
                        conversation_key: lane.conversation_key,
                        text: text.to_owned(),
                        previous_session: lane.omp_session_id,
                    }
                }
                LaneMessage::Unknown => {
                    return Err(refused(E_LANE_UNKNOWN, "no lane has this id"));
                }
                LaneMessage::Busy => {
                    return Err(refused(
                        E_LANE_BUSY,
                        "the lane is still running; wait for its result",
                    ));
                }
                LaneMessage::Limit => {
                    return Err(refused(
                        E_LANE_LIMIT,
                        &format!("{MAX_RUNNING_LANES} lanes are already running"),
                    ));
                }
            }
        };
        self.spawn_worker(&mut set, job);
        Ok(json!({ "lane_id": id, "accepted": true })
            .to_string()
            .into())
    }

    fn lane_status(
        &self,
        arguments: &Map<String, Value>,
    ) -> Result<omp_rpc::HostToolOutput, HostToolError> {
        let wanted = match arguments.get("lane_id") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .ok_or_else(|| refused(E_LANE_ARGS, "lane_id must be an integer"))?,
            ),
        };
        let lanes = lock(&self.store)
            .lanes()
            .map_err(|e| refused(e.code(), &e.to_string()))?;
        let listed: Vec<Value> = lanes
            .into_iter()
            .filter(|lane| wanted.is_none_or(|id| lane.id == id))
            .map(|lane| {
                json!({
                    "lane_id": lane.id,
                    "conversation": lane.conversation_key,
                    "state": lane.state.as_str(),
                    "error": lane.error,
                    "prompts": lane.prompts,
                    "task": lane.task.chars().take(STATUS_TASK_CHARS).collect::<String>(),
                })
            })
            .collect();
        if wanted.is_some() && listed.is_empty() {
            return Err(refused(E_LANE_UNKNOWN, "no lane has this id"));
        }
        Ok(Value::Array(listed).to_string().into())
    }

    /// Starts the worker thread for `job` and registers it, under the caller's hold of
    /// `inner`. When no thread can be started, the prompt is reported as failed.
    fn spawn_worker(self: &Arc<Self>, set: &mut LaneSet, job: Job) {
        let (id, seq) = (job.id, job.seq);
        let key = job.conversation_key.clone();
        let manager = Arc::clone(self);
        match std::thread::Builder::new()
            .name(format!("omp-lane-{id}"))
            .spawn(move || manager.run_worker(job))
        {
            Ok(worker) => {
                set.children.insert(id, LaneChild { kill: None, worker });
                tracing::info!(lane = id, seq, "lane started");
            }
            Err(e) => {
                tracing::warn!(lane = id, seq, error = %e, "could not start a lane worker");
                let outcome = Outcome::Failed {
                    code: E_OMP_SPAWN,
                    detail: e.to_string(),
                };
                if self.write_result(set, id, seq, &key, Err(outcome)) {
                    self.wake.notify_one();
                }
            }
        }
    }

    /// One lane prompt: open the child, send the prompt, map the result, close the
    /// child; then the final section under `inner`.
    fn run_worker(self: Arc<Self>, job: Job) {
        let mut pid = None;
        let outcome = self.run_prompt(&job, &mut pid);
        let written = {
            let mut set = lock(&self.inner);
            let written = self.write_result(&set, job.id, job.seq, &job.conversation_key, outcome);
            if let Some(mut child) = set.children.remove(&job.id) {
                if let Some(group) = child.kill.as_mut() {
                    // The child was reaped by `close` above.
                    group.disarm();
                }
            }
            if let Some(pid) = pid {
                self.pids.send_modify(|pids| pids.retain(|p| *p != pid));
            }
            written
        };
        if written {
            self.wake.notify_one();
        }
    }

    /// `Err` is a failure before the prompt was sent (the child did not open).
    fn run_prompt(&self, job: &Job, pid: &mut Option<u32>) -> Result<Outcome, Outcome> {
        let name = format!("lane-{}", job.id);
        let session_dir = omp::lane_session_dir(&self.config, job.id);
        let command = omp::private_dir(&session_dir)
            .and_then(|()| {
                omp::child_command(
                    &self.config,
                    omp::omp_args(&self.config, &session_dir, ""),
                    &omp::stderr_log_path(&self.config, &name),
                    &self.extra_env,
                )
            })
            .map_err(|e| Outcome::Failed {
                code: E_OMP_SPAWN,
                detail: e.to_string(),
            })?;
        let OpenedChild {
            client, session_id, ..
        } = omp::open_child(
            &name,
            command,
            Vec::new(),
            session_dir.to_string_lossy().into_owned(),
            job.previous_session.clone(),
            |group| {
                let mut set = lock(&self.inner);
                if set.stopping {
                    return false;
                }
                let Some(child) = set.children.get_mut(&job.id) else {
                    return false;
                };
                if let Some(spawned) = group.pid() {
                    *pid = Some(spawned);
                    self.pids.send_modify(|pids| pids.push(spawned));
                }
                child.kill = Some(group);
                true
            },
            |_| {},
        )?;
        if let Err(e) = lock(&self.store).set_lane_session_id(job.id, &session_id) {
            tracing::warn!(
                lane = job.id,
                code = e.code(),
                "could not store the lane's omp session id"
            );
        }
        let prompt = PromptCommand {
            message: job.text.clone(),
            images: None,
            streaming_behavior: None,
        };
        let outcome = omp::map_result(client.prompt_and_wait(&prompt, self.turn_timeout));
        omp::close(&name, client);
        Ok(outcome)
    }

    /// Stores the lane prompt's result row and the lane's new state, unless the shutdown
    /// kill ended the prompt (then the lane stays `running` for `recover_on_start`). A
    /// prompt that ended on its own is written even while stopping. Returns whether a
    /// row was written.
    fn write_result(
        &self,
        set: &LaneSet,
        id: i64,
        seq: i64,
        conversation_key: &str,
        outcome: Result<Outcome, Outcome>,
    ) -> bool {
        let killed = set.stopping
            && match &outcome {
                Err(_) | Ok(Outcome::NeedsCheck { .. }) => true,
                Ok(_) => false,
            };
        if killed {
            tracing::info!(
                lane = id,
                seq,
                "lane stopped by shutdown; left for recovery"
            );
            return false;
        }
        let outcome = outcome.unwrap_or_else(|failed| failed);
        let (text, state) = match &outcome {
            Outcome::Reply(text) => (
                lead::lane_result(conversation_key, id, &LaneResult::Text(text)),
                LaneState::Idle,
            ),
            Outcome::NoAgent => (
                lead::lane_result(conversation_key, id, &LaneResult::Text(EMPTY_REPLY)),
                LaneState::Idle,
            ),
            Outcome::Failed { code, .. } => (
                lead::lane_result(conversation_key, id, &LaneResult::Failed(code)),
                LaneState::Failed,
            ),
            Outcome::NeedsCheck { code, .. } => (
                lead::lane_result(conversation_key, id, &LaneResult::Failed(code)),
                LaneState::NeedsCheck,
            ),
        };
        let code = outcome.code();
        match lock(&self.store).finish_lane(id, seq, &text, state, code) {
            Ok(written) => {
                tracing::info!(
                    lane = id,
                    seq,
                    state = state.as_str(),
                    code,
                    "lane finished"
                );
                written
            }
            Err(e) => {
                tracing::error!(
                    lane = id,
                    seq,
                    code = e.code(),
                    "could not store the lane result"
                );
                false
            }
        }
    }
}
