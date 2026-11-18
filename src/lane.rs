//! Lanes: separate headless omp children started by the lead through host tools. Messenger-agnostic.
//!
//! The lead calls `lane_start`, `lane_message` and `lane_status` (registered on the lead
//! child next to `send_file`). A lane is its own omp RPC child with no appended system
//! prompt; it runs one prompt on a worker thread and closes. A lane with a done line gets
//! exactly one host tool, `lane_done`: the lane claims its work is done, moraebot runs
//! the Done checks inside that call and refuses a claim that fails them. Its result is
//! stored as one queued inbox row (`platform = LANE_PLATFORM`) in the same transaction as
//! the lane's new state, and the dispatcher is woken; the lead answers it in a normal
//! turn. Handlers never wait for lane work.
//!
//! Lock order is claim -> `inner` -> store everywhere; nothing takes a claim lock while
//! holding `inner`. A handler holds `inner` from the `stopping` check until the new
//! worker is registered, and a worker writes its result and removes its own registration
//! under one hold of `inner`, so at most one worker per lane is ever registered and
//! `shutdown_all` sees every lane child that exists. If omp ever ended a turn while a
//! `lane_done` call was still running, `run_prompt` waits on the claim lock for that one
//! check; it holds no lock while it waits, so this is not a deadlock.
//!
//! Shutdown kills every lane child first, then every running check, then waits until
//! each killed check was reaped: a check's refusal never reaches a live lane, so a turn
//! cut during a check is always left for `recover_on_start`.
//!
//! A lane started with `repo` works in its own git worktree,
//! `<work dir>/.moraebot-lanes/lane-<id>` on branch `moraebot/lane-<id>`, created before
//! its first child starts and reused by later prompts; its git commands are tracked and
//! killed on shutdown like Done checks. Its `lane_done` claims first read the worktree's git
//! state: uncommitted tracked changes, and commits on no remote when it has one, refuse the
//! claim like a failing Done check.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use omp_rpc::{HostTool, HostToolContext, HostToolError, PromptCommand, ToolLoadMode};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};

use crate::config::Config;
use crate::done::{self, DoneLine};
use crate::lead::{self, LaneResult};
use crate::omp::{
    self, ABORT_JOIN_BOUND, E_OMP_SPAWN, EMPTY_REPLY, GroupKill, OpenedChild, Outcome, TurnContext,
    lock,
};
use crate::store::{LaneMessage, LaneStart, LaneState, Store};
use crate::worktree::{self, WorktreeError};

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
/// The lane was closed with `lane_close` and takes no more work.
pub const E_LANE_CLOSED: &str = "E_LANE_CLOSED";

/// `lane_status` cuts each task to this many characters.
const STATUS_TASK_CHARS: usize = 200;

/// One registered lane worker: its child's process group once spawned, and its thread.
struct LaneChild {
    kill: Option<GroupKill>,
    worker: JoinHandle<()>,
    /// The prompt (job) this worker runs.
    seq: i64,
}

#[derive(Default)]
struct LaneSet {
    /// Set by `stop_accepting`: no lane starts and no child is kept from then on.
    stopping: bool,
    children: HashMap<i64, LaneChild>,
    /// Running check and git pids -> `(lane id, seq)` of the job that started it.
    checks: HashMap<u32, (i64, i64)>,
}

/// One lane prompt for a worker thread.
struct Job {
    id: i64,
    seq: i64,
    conversation_key: String,
    text: String,
    previous_session: Option<String>,
    done: Option<DoneLine>,
    /// The canonical git checkout of the lane's worktree; `None` = the shared work folder.
    repo: Option<PathBuf>,
}

/// Where a job's Done checks run (`work`) and write their output (`out`).
struct CheckDirs {
    work: PathBuf,
    out: PathBuf,
    /// Whether `work` is the lane's own worktree, whose git state is checked before the Done
    /// signals.
    git: bool,
}

/// What the lane's `lane_done` calls of one job established; the last call decides.
enum Claim {
    None,
    Failed { receipt: String },
    Accepted { receipt: String },
}

/// A lane prompt that ran, with its claim state applied.
enum LaneOutcome {
    Plain(Outcome),
    Done { text: String, receipt: String },
    Unmet { text: String, receipt: String },
}

pub struct LaneManager {
    config: Config,
    store: Arc<Mutex<Store>>,
    wake: Arc<Notify>,
    turn: Arc<Mutex<Option<TurnContext>>>,
    pub(crate) turn_timeout: Duration,
    /// Kill deadline of one Done check command.
    pub(crate) check_timeout: Duration,
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
            check_timeout: done::CHECK_TIMEOUT,
            inner: Mutex::default(),
            pids: watch::channel(Vec::new()).0,
            extra_env: Vec::new(),
        }
    }

    /// Notified after a lane result row was stored.
    pub(crate) fn wake(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    /// Pids of lane children and of Done check commands that are spawned and not yet
    /// reaped.
    pub fn running_pids(&self) -> watch::Receiver<Vec<u32>> {
        self.pids.subscribe()
    }

    /// From now on `lane_start` and `lane_message` refuse with `E_LANE_STOPPING`.
    pub fn stop_accepting(&self) {
        lock(&self.inner).stopping = true;
    }

    /// Stops accepting, then kills every lane child (including one still starting), then
    /// every running check, waits until each killed check was reaped, and joins each
    /// worker; every wait is bounded by `ABORT_JOIN_BOUND`. A lane the kill ended keeps
    /// its `running` row for `recover_on_start`.
    pub async fn shutdown_all(&self) {
        let mut pids = self.pids.subscribe();
        let (children, checks): (Vec<(i64, LaneChild)>, Vec<u32>) = {
            let mut set = lock(&self.inner);
            set.stopping = true;
            (
                set.children.drain().collect(),
                set.checks.keys().copied().collect(),
            )
        };
        for (_, child) in &children {
            if let Some(group) = &child.kill {
                group.kill();
            }
        }
        // Children first: a check's refusal can never reach a live lane.
        for pid in &checks {
            omp::kill_group(*pid);
        }
        if !checks.is_empty() {
            let reaped = pids.wait_for(|running| !running.iter().any(|p| checks.contains(p)));
            if !matches!(
                tokio::time::timeout(ABORT_JOIN_BOUND, reaped).await,
                Ok(Ok(_))
            ) {
                tracing::error!(checks = checks.len(), "a done check did not stop in time");
            }
        }
        for (id, LaneChild { kill, worker, .. }) in children {
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
                 the shared work folder, or with `repo` in its own git worktree. `task` must \
                 be self-contained; the lane does not see \
                 the conversation. `done` is the Done line moraebot checks when the lane \
                 claims it is done. Returns the lane id at once; the lane's result comes \
                 back to you later as a new message.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "The self-contained task."},
                        "done": {
                            "type": "string",
                            "description": "Done line: `command [...]` and/or `file <path>`, joined with `;`."
                        },
                        "repo": {
                            "type": "string",
                            "description": "Optional: a git checkout (absolute, ~/..., or relative to the work folder). The lane then works in its own worktree of it."
                        }
                    },
                    "required": ["task", "done"],
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
        let line = arguments
            .get("done")
            .and_then(Value::as_str)
            .filter(|line| !line.trim().is_empty())
            .ok_or_else(|| refused(E_LANE_ARGS, "done must be a non-empty string"))?;
        let line = DoneLine::parse(line)
            .map_err(|error| refused(done::E_DONE_SYNTAX, &error.to_string()))?;
        let canonical = line.to_string();
        let repo = self.repo_arg(arguments)?;
        let repo_text = repo
            .as_deref()
            .map(|repo| repo.to_string_lossy().into_owned());
        let conversation_key = lock(&self.turn)
            .as_ref()
            .map(|turn| turn.conversation_key.clone())
            .ok_or_else(|| refused(E_LANE_ARGS, "no active turn"))?;
        let mut set = lock(&self.inner);
        if set.stopping {
            return Err(refused(E_LANE_STOPPING, "the gateway is stopping"));
        }
        let started = lock(&self.store)
            .start_lane(
                tool_call_id,
                &conversation_key,
                task,
                Some(&canonical),
                repo_text.as_deref(),
                MAX_RUNNING_LANES,
            )
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
                        done: Some(line),
                        repo,
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

    /// `lane_start`'s optional `repo`: missing or `null` is `None`; otherwise a non-empty
    /// string naming an existing folder (`~/` from `HOME`, absolute, else relative to the
    /// work folder), returned canonical.
    fn repo_arg(&self, arguments: &Map<String, Value>) -> Result<Option<PathBuf>, HostToolError> {
        let repo = match arguments.get("repo") {
            None | Some(Value::Null) => return Ok(None),
            Some(value) => value
                .as_str()
                .filter(|repo| !repo.trim().is_empty())
                .ok_or_else(|| refused(E_LANE_ARGS, "repo must be a non-empty string"))?,
        };
        let not_a_folder = || refused(worktree::E_LANE_REPO, "repo is not an existing folder");
        let path = match repo.strip_prefix("~/") {
            Some(rest) => {
                PathBuf::from(std::env::var_os("HOME").ok_or_else(not_a_folder)?).join(rest)
            }
            None if Path::new(repo).is_absolute() => PathBuf::from(repo),
            None => self.config.work_dir.join(repo),
        };
        let canonical = std::fs::canonicalize(path).map_err(|_| not_a_folder())?;
        if !canonical.is_dir() {
            return Err(not_a_folder());
        }
        Ok(Some(canonical))
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
            let lane = store
                .lane(id)
                .map_err(store_err)?
                .ok_or_else(|| refused(E_LANE_UNKNOWN, "no lane has this id"))?;
            let line = lane
                .done
                .as_deref()
                .map(DoneLine::parse)
                .transpose()
                .map_err(|error| refused(done::E_DONE_SYNTAX, &error.to_string()))?;
            match store
                .message_lane(id, MAX_RUNNING_LANES)
                .map_err(store_err)?
            {
                LaneMessage::Seq(seq) => Job {
                    id,
                    seq,
                    conversation_key: lane.conversation_key,
                    text: text.to_owned(),
                    previous_session: lane.omp_session_id,
                    done: line,
                    repo: lane.repo.map(PathBuf::from),
                },
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
                LaneMessage::Closed => {
                    return Err(refused(E_LANE_CLOSED, "the lane is closed"));
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
                    "done": lane.done,
                    "repo": lane.repo,
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
                set.children.insert(
                    id,
                    LaneChild {
                        kill: None,
                        worker,
                        seq,
                    },
                );
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
            // A check outlives no job: `close` does not join a running tool handler.
            for (check, owner) in &set.checks {
                if *owner == (job.id, job.seq) {
                    omp::kill_group(*check);
                }
            }
            if let Some(mut child) = set.children.remove(&job.id)
                && let Some(group) = child.kill.as_mut()
            {
                // The child was reaped by `close` above.
                group.disarm();
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

    /// Registers `pid` (a check or git command) as running for job `(id, seq)`; `false`
    /// when moraebot is stopping, the call was cancelled or the job is no longer current.
    fn track(&self, pid: u32, (id, seq): (i64, i64), cancelled: bool) -> bool {
        let mut set = lock(&self.inner);
        let current = set.children.get(&id).is_some_and(|child| child.seq == seq);
        if set.stopping || cancelled || !current {
            return false;
        }
        set.checks.insert(pid, (id, seq));
        self.pids.send_modify(|pids| pids.push(pid));
        true
    }

    /// Forgets `pid` once its command was reaped.
    fn untrack(&self, pid: u32) {
        let mut set = lock(&self.inner);
        set.checks.remove(&pid);
        self.pids.send_modify(|pids| pids.retain(|p| *p != pid));
    }

    /// `<work dir>` canonical (else absolute, else as given): the base of lane worktrees.
    fn lanes_root(&self) -> PathBuf {
        std::fs::canonicalize(&self.config.work_dir)
            .or_else(|_| std::path::absolute(&self.config.work_dir))
            .unwrap_or_else(|_| self.config.work_dir.clone())
    }

    /// The folder job `job` works in: the work folder, or for a lane with a repo its
    /// worktree, made sure of by `worktree::ensure` before any child starts.
    fn job_cwd(&self, job: &Job) -> Result<PathBuf, Outcome> {
        let Some(repo) = &job.repo else {
            return Ok(self.config.work_dir.clone());
        };
        let (id, seq) = (job.id, job.seq);
        let folder = worktree::folder(&self.lanes_root(), id);
        let git_dir = self
            .config
            .home
            .join("lanes")
            .join(id.to_string())
            .join("git");
        let fail = |code: &'static str, detail: String| {
            tracing::warn!(lane = id, seq, code, "lane worktree is not ready");
            Outcome::Failed { code, detail }
        };
        omp::private_dir(&git_dir).map_err(|e| fail(worktree::E_LANE_WORKTREE, e.to_string()))?;
        let env = omp::check_env(&self.extra_env);
        let opts = done::RunOpts {
            out_dir: &git_dir,
            env: &env,
            timeout: worktree::GIT_TIMEOUT,
        };
        match worktree::ensure(
            repo,
            &folder,
            id,
            &opts,
            |pid| self.track(pid, (id, seq), false),
            |pid| self.untrack(pid),
        ) {
            Ok(()) => Ok(folder),
            Err(WorktreeError::Failed { code, detail }) => Err(fail(code, detail)),
            Err(WorktreeError::Stopped) => {
                Err(fail(E_LANE_STOPPING, "the gateway is stopping".to_owned()))
            }
        }
    }

    /// The `lane_done` tool of one job: its handler checks `line` in `work_dir` and
    /// records the claim.
    fn lane_done_tool(
        self: &Arc<Self>,
        job: &Job,
        line: &DoneLine,
        claim: &Arc<Mutex<Claim>>,
        work_dir: &Path,
    ) -> HostTool {
        let (id, seq) = (job.id, job.seq);
        let check_dir = self
            .config
            .home
            .join("lanes")
            .join(id.to_string())
            .join("check")
            .join(seq.to_string());
        if omp::private_dir(&check_dir).is_err() {
            tracing::warn!(
                lane = id,
                code = done::E_CHECK_SPAWN,
                "could not create the lane's check folder"
            );
        }
        let dirs = CheckDirs {
            work: work_dir.to_path_buf(),
            out: check_dir,
            git: job.repo.is_some(),
        };
        let manager = Arc::clone(self);
        let line = line.clone();
        let claim = Arc::clone(claim);
        HostTool::new(
            done::LANE_DONE_TOOL,
            "Claim that your work is done. moraebot runs this lane's Done checks now; a \
             refusal lists what still fails.",
            schema(json!({
                "type": "object",
                "properties": {
                    "evidence": {
                        "type": "string",
                        "description": "What you did and how you checked it."
                    }
                },
                "required": ["evidence"],
                "additionalProperties": false
            })),
            move |arguments, ctx| {
                manager.lane_done(&arguments, ctx, (id, seq), &line, &dirs, &claim)
            },
        )
        .load_mode(ToolLoadMode::Essential)
    }

    /// Handles one `lane_done` call on its own client thread. The claim lock is held for
    /// the whole call, so parallel calls of one lane run one after another.
    fn lane_done(
        &self,
        arguments: &Map<String, Value>,
        ctx: &HostToolContext,
        (id, seq): (i64, i64),
        line: &DoneLine,
        dirs: &CheckDirs,
        claim: &Mutex<Claim>,
    ) -> Result<omp_rpc::HostToolOutput, HostToolError> {
        let evidence = arguments
            .get("evidence")
            .and_then(Value::as_str)
            .filter(|evidence| !evidence.trim().is_empty())
            .ok_or_else(|| refused(E_LANE_ARGS, "evidence must be a non-empty string"))?;
        let stopping = || refused(E_LANE_STOPPING, "the gateway is stopping");
        let mut claim = lock(claim);
        if lock(&self.inner).stopping {
            return Err(stopping());
        }
        let env = omp::check_env(&self.extra_env);
        let mut checks = Vec::new();
        if dirs.git {
            let opts = done::RunOpts {
                out_dir: &dirs.out,
                env: &env,
                timeout: worktree::GIT_TIMEOUT,
            };
            match worktree::git_checks(
                &dirs.work,
                &opts,
                |pid| self.track(pid, (id, seq), ctx.is_cancelled()),
                |pid| self.untrack(pid),
            ) {
                Ok(git) => checks = git,
                Err(done::Stopped) => return Err(stopping()),
            }
        }
        let signals = done::run(
            line,
            &dirs.work,
            &dirs.out,
            &env,
            self.check_timeout,
            |pid| self.track(pid, (id, seq), ctx.is_cancelled()),
            |pid| self.untrack(pid),
        );
        match signals {
            Ok(signals) if !lock(&self.inner).stopping => checks.extend(signals),
            _ => return Err(stopping()),
        }
        let receipt = done::receipt(evidence, &checks);
        if done::all_passed(&checks) {
            tracing::info!(
                lane = id,
                seq,
                signals = checks.len(),
                "lane done claim accepted"
            );
            *claim = Claim::Accepted { receipt };
            return Ok(format!(
                "{}: every Done check passed. End your turn now with a short summary of \
                 what you did.",
                done::ACCEPTED
            )
            .into());
        }
        let failing: Vec<&str> = checks.iter().filter_map(|check| check.code).collect();
        tracing::info!(
            lane = id,
            seq,
            signals = checks.len(),
            codes = ?failing,
            "lane done claim refused"
        );
        *claim = Claim::Failed { receipt };
        Err(refused(done::E_DONE_UNMET, &done::not_done(&checks)))
    }

    /// `Err` is a failure before the prompt was sent (the child did not open).
    fn run_prompt(
        self: &Arc<Self>,
        job: &Job,
        pid: &mut Option<u32>,
    ) -> Result<LaneOutcome, Outcome> {
        let cwd = self.job_cwd(job)?;
        let name = format!("lane-{}", job.id);
        let session_dir = omp::lane_session_dir(&self.config, job.id);
        let command = omp::private_dir(&session_dir)
            .and_then(|()| {
                omp::child_command(
                    &self.config,
                    omp::omp_args(&self.config, &cwd, &session_dir, ""),
                    &omp::stderr_log_path(&self.config, &name),
                    &self.extra_env,
                )
            })
            .map_err(|e| Outcome::Failed {
                code: E_OMP_SPAWN,
                detail: e.to_string(),
            })?;
        let claim = Arc::new(Mutex::new(Claim::None));
        let tools = match &job.done {
            Some(line) => vec![self.lane_done_tool(job, line, &claim, &cwd)],
            None => Vec::new(),
        };
        let OpenedChild {
            client, session_id, ..
        } = omp::open_child(
            &name,
            command,
            tools,
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
        let message = match (&job.done, &job.repo) {
            (Some(line), Some(_)) => {
                job.text.clone() + &done::lane_footer(line) + worktree::GIT_FOOTER
            }
            (Some(line), None) => job.text.clone() + &done::lane_footer(line),
            (None, _) => job.text.clone(),
        };
        let prompt = PromptCommand {
            message,
            images: None,
            streaming_behavior: None,
        };
        let outcome = omp::map_result(client.prompt_and_wait(&prompt, self.turn_timeout));
        let replied = match (&outcome, &job.done) {
            (Outcome::Reply(text), Some(_)) => Some(text.clone()),
            (Outcome::NoAgent, Some(_)) => Some(EMPTY_REPLY.to_owned()),
            _ => None,
        };
        let outcome = match replied {
            None => LaneOutcome::Plain(outcome),
            Some(text) => match &*lock(&claim) {
                Claim::None => LaneOutcome::Plain(outcome),
                Claim::Accepted { receipt } => LaneOutcome::Done {
                    text,
                    receipt: receipt.clone(),
                },
                Claim::Failed { receipt } => LaneOutcome::Unmet {
                    text,
                    receipt: receipt.clone(),
                },
            },
        };
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
        outcome: Result<LaneOutcome, Outcome>,
    ) -> bool {
        let killed = set.stopping
            && matches!(
                &outcome,
                Err(_) | Ok(LaneOutcome::Plain(Outcome::NeedsCheck { .. }))
            );
        if killed {
            tracing::info!(
                lane = id,
                seq,
                "lane stopped by shutdown; left for recovery"
            );
            return false;
        }
        let outcome = outcome.unwrap_or_else(LaneOutcome::Plain);
        let (result, state, code) = match &outcome {
            LaneOutcome::Plain(plain) => {
                let (result, state) = match plain {
                    Outcome::Reply(text) => (LaneResult::Text(text), LaneState::Idle),
                    Outcome::NoAgent => (LaneResult::Text(EMPTY_REPLY), LaneState::Idle),
                    Outcome::Failed { code, .. } => (LaneResult::Failed(code), LaneState::Failed),
                    Outcome::NeedsCheck { code, .. } => {
                        (LaneResult::Failed(code), LaneState::NeedsCheck)
                    }
                };
                (result, state, plain.code())
            }
            LaneOutcome::Done { text, receipt } => {
                (LaneResult::Done { text, receipt }, LaneState::Idle, None)
            }
            LaneOutcome::Unmet { text, receipt } => (
                LaneResult::Unmet { text, receipt },
                LaneState::Failed,
                Some(done::E_DONE_UNMET),
            ),
        };
        let text = lead::lane_result(conversation_key, id, &result);
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
