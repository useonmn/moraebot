//! The memory observer (#39, design 4.1): turns the memory jobs the store queues into saved
//! entries without the lead's help.
//!
//! - A `group` job (a settled, failed or restart-cut lead turn) judges the owner's own
//!   sentences of its owner rows. An entry is always one whole owner sentence, word for word,
//!   with `prov: owner` and its row and sentence number; jev only picks which sentence and
//!   which file.
//! - A `lane` job judges a lane result once, a `subagent` job one subagent output file. Their
//!   sentences pass the excerpt filter ([`ExcerptFilter`]) before anything is sent, and are
//!   saved as reference (`lane` or `subagent`), never in `core/`.
//! - A `scan` job, and every group and lane job, first scans a session folder for subagent
//!   output files ([`Observer::scan_root`]); a new or changed file becomes a `subagent` job.
//!
//! The observer runs beside the dispatcher, never inside it: a jev call that waits or fails
//! never holds the owner's memory commands, and a bug here never stops the gateway. A panic in
//! a job counts as one failed attempt of that job; a panic elsewhere in a pass ends only that
//! pass, is logged as [`E_OBSERVER_PANIC`] and shows in [`Observer::status_line`] until a pass
//! ends normally again. It never sleeps between retries either: when jev refuses a call the pass stops, every later job stays
//! pending, and [`Observer::run`] calls [`Observer::run_once`] again later. Entries go to the
//! memory writer as one batch per pass ([`MemoryHandle::apply_batch`], one git commit) as
//! [`Op::AddUnique`], so a job processed twice adds nothing. Saves are never posted in a
//! conversation (#56): the memory writer logs each one, and the owner lists them on request.

use std::collections::HashSet;
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, watch};

use crate::config::Config;
use crate::jev::{Answer, Jev, JevError, JevStatus, Now, Question, Request};
use crate::jev::{E_JEV_REQUEST, E_JEV_RESPONSE};
use crate::lead;
use crate::memory::{
    self, Bucket, E_MEMORY_STOPPED, ExcerptFilter, MAX_ENTRY_BYTES, MemoryHandle, NewEntry, Op,
    Outcome, Prov, Redactor, Source,
};
use crate::memory_commands::{says_always, says_boundary};
use crate::omp;
use crate::store::{
    GroupJob, JobEnd, JobKind, LaneJob, LogLevel, LogRow, MemoryJob, Settled, Store, StoreError,
    SubagentJob, ToolUse,
};

/// A job settled after it was processed (`detail` holds the counts).
pub const M_JOB_DONE: &str = "M_JOB_DONE";
/// A job settled without a jev request because jev is off.
pub const M_JEV_OFF: &str = "M_JEV_OFF";
/// A subagent file changed or vanished before its job ran; the changed file has its own job.
pub const M_ARTIFACT_CHANGED: &str = "M_ARTIFACT_CHANGED";
/// A subagent output file was skipped: a symlink, not a regular file, too large, outside its
/// session folder, or a task name that is not usable as a source.
pub const W_SUBAGENT_SKIPPED: &str = "W_SUBAGENT_SKIPPED";
/// A settled lead turn started a subagent, but no subagent output file was found where omp
/// 18.22.2 writes it: omp's session layout may have changed.
pub const E_SUBAGENT_LAYOUT: &str = "E_SUBAGENT_LAYOUT";
/// More than [`BACKLOG_WARN`] memory jobs are pending.
pub const W_MEMORY_BACKLOG: &str = "W_MEMORY_BACKLOG";
/// One entry of a job could not be written (too long, unusable bucket name).
pub const W_MEMORY_WRITE: &str = "W_MEMORY_WRITE";
/// A job could not be processed (unreadable row or file); counts as one attempt.
pub const E_OBSERVER_JOB: &str = "E_OBSERVER_JOB";
/// Processing a job panicked (counts as one attempt), or a pass panicked outside a job (the
/// pass ends, the observer reports itself down until a pass ends normally).
pub const E_OBSERVER_PANIC: &str = "E_OBSERVER_PANIC";

/// Pending `lane`, `subagent` and `scan` jobs older than this expire (7 days).
const JOB_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
/// Jobs taken per round of a pass.
const BATCH: usize = 16;
/// How long [`Observer::run`] waits when nothing wakes it.
const IDLE_TICK: Duration = Duration::from_secs(60);
/// Pending jobs above which one [`W_MEMORY_BACKLOG`] row is written per day.
const BACKLOG_WARN: usize = 10_000;
/// The largest subagent output file that is read.
pub const MAX_ARTIFACT_BYTES: u64 = 256 * 1024;
/// How deep below a session root a subagent artifact folder may be.
const MAX_SCAN_DEPTH: usize = 3;
/// Windows judged per owner row, and per lane or subagent text.
const OWNER_WINDOWS: usize = 10;
const EXCERPT_WINDOWS: usize = 5;
/// Shorter sentences are never candidates.
const MIN_SENTENCE_CHARS: usize = 4;
/// The `why` of every observer entry, read in the commit subject.
const WHY: &str = "observed";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// Nothing that can be done now is pending.
    Idle,
    /// jev refused a call (or memory stopped); jobs stay pending. `until` is when jev's gate
    /// opens again, `None` when it stays closed until restart.
    Waiting { until: Option<i64> },
}

/// Which text a jev request judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Owner,
    Lane,
    /// A subagent of a lane (`true`) or of the lead (`false`).
    Subagent {
        lane: bool,
    },
}

impl Kind {
    fn state_kind(self) -> &'static str {
        match self {
            Self::Owner => "owner_message",
            Self::Lane => "lane_result",
            Self::Subagent { .. } => "subagent_report",
        }
    }

    /// The `bucket` options, in their fixed order.
    fn buckets(self) -> &'static [&'static str] {
        match self {
            Self::Owner => &["thread", "project", "core", "boundary", "none"],
            Self::Lane | Self::Subagent { lane: true } => &["thread", "project"],
            Self::Subagent { lane: false } => &["project"],
        }
    }
}

/// What processing one job produced.
#[derive(Debug)]
struct Work {
    code: &'static str,
    entries: Vec<NewEntry>,
    requests: u32,
    tokens: u64,
    model: Option<String>,
}

impl Work {
    fn new() -> Self {
        Self {
            code: M_JOB_DONE,
            entries: Vec::new(),
            requests: 0,
            tokens: 0,
            model: None,
        }
    }
}

/// Why a job did not finish.
enum Failure {
    /// jev refused or could not answer: stop the pass, keep the job pending.
    Jev(JevError),
    /// A processing error that is not jev's: one failed attempt.
    Job(&'static str),
}

impl From<StoreError> for Failure {
    fn from(err: StoreError) -> Self {
        tracing::warn!(error = %err, "memory job could not read the store");
        Self::Job(E_OBSERVER_JOB)
    }
}

/// jev's pick for one window: the sentence number, the sentence and the bucket option.
struct Pick {
    n: u32,
    sentence: String,
    bucket: String,
}

pub struct Observer {
    config: Config,
    store: Arc<Mutex<Store>>,
    memory: MemoryHandle,
    jev: Arc<Jev>,
    redactor: Redactor,
    filter: ExcerptFilter,
    now: Now,
    omp_version: String,
    progress: watch::Sender<u64>,
    started: AtomicBool,
    pruned_day: Mutex<Option<i64>>,
    layout_logged: AtomicBool,
    pause_logged: AtomicBool,
    memory_stopped: AtomicBool,
    /// The last pass panicked outside a job.
    down: AtomicBool,
}

impl Observer {
    /// An observer of `store`'s memory jobs that writes through `memory`, asks `jev`, redacts
    /// with `redactor` and filters lane and subagent sentences with `config`'s
    /// `[memory.filter]`. `now` is the clock in unix seconds.
    pub fn new(
        config: &Config,
        store: Arc<Mutex<Store>>,
        memory: MemoryHandle,
        jev: Arc<Jev>,
        redactor: Redactor,
        now: Now,
    ) -> Self {
        Self {
            filter: ExcerptFilter::from_config(&config.memory.filter),
            config: config.clone(),
            store,
            memory,
            jev,
            redactor,
            now,
            omp_version: "unknown".to_owned(),
            progress: watch::channel(0).0,
            started: AtomicBool::new(false),
            pruned_day: Mutex::new(None),
            layout_logged: AtomicBool::new(false),
            pause_logged: AtomicBool::new(false),
            memory_stopped: AtomicBool::new(false),
            down: AtomicBool::new(false),
        }
    }

    /// The omp version written with every settled job (`unknown` until set).
    pub fn with_omp_version(mut self, version: &str) -> Self {
        self.omp_version = version.to_owned();
        self
    }

    /// The count of jobs settled so far; tests wait on it.
    pub fn progress(&self) -> watch::Receiver<u64> {
        self.progress.subscribe()
    }

    /// The jev client this observer asks; the dispatcher's memory request classifier shares it,
    /// and with it the gate.
    pub fn jev(&self) -> &Jev {
        &self.jev
    }

    /// The redactor of the memory store, which also hides the jev key.
    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// One line for the owner: jev's state and how many jobs wait, or that the observer is
    /// down after a panic.
    pub fn status_line(&self) -> String {
        let backlog = lock(&self.store).memory_backlog().unwrap_or_default();
        let waiting = backlog.groups + backlog.other;
        if self.down.load(Ordering::SeqCst) {
            return format!("Observer: down ({E_OBSERVER_PANIC}), retrying, {waiting} waiting");
        }
        match self.jev.status() {
            JevStatus::Off => format!("Observer: jev off, {waiting} waiting"),
            JevStatus::Ready => format!("Observer: jev ok, {waiting} waiting"),
            JevStatus::Waiting { .. } | JevStatus::Open { .. } => {
                format!("Observer: jev busy, retrying, {waiting} waiting")
            }
            JevStatus::Paused { code } => format!("Observer: paused ({code}), {waiting} waiting"),
        }
    }

    /// Runs passes until `stop` resolves or the memory writer stops: after each pass it waits
    /// for the store's wake, jev's gate time or the idle tick.
    pub async fn run(self: Arc<Self>, wake: Arc<Notify>, stop: impl Future<Output = ()>) {
        tokio::pin!(stop);
        loop {
            let pass = tokio::select! {
                pass = self.run_once() => pass,
                () = &mut stop => return,
            };
            if self.memory_stopped.load(Ordering::SeqCst) {
                tracing::error!(code = E_MEMORY_STOPPED, "memory observer stopped");
                return;
            }
            let wait = match pass {
                Pass::Idle | Pass::Waiting { until: None } => IDLE_TICK,
                Pass::Waiting { until: Some(at) } => {
                    let seconds = (at - (self.now)()).clamp(1, IDLE_TICK.as_secs() as i64);
                    Duration::from_secs(seconds as u64)
                }
            };
            tokio::select! {
                () = wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
                () = &mut stop => return,
            }
        }
    }

    /// Does everything that can be done now: the start scan (first call only), expiry, the
    /// daily prune, then the pending jobs in id order until none is left or jev refuses a
    /// call. A panic anywhere in the pass ends only the pass: it is logged as
    /// [`E_OBSERVER_PANIC`], [`Self::status_line`] reports the observer down, and the pass
    /// waits like a refused one.
    pub async fn run_once(&self) -> Pass {
        match AssertUnwindSafe(self.pass()).catch_unwind().await {
            Ok(pass) => {
                self.down.store(false, Ordering::SeqCst);
                pass
            }
            Err(_) => {
                tracing::error!(code = E_OBSERVER_PANIC, "memory observer pass panicked");
                self.down.store(true, Ordering::SeqCst);
                // The injected clock may be what panicked.
                let at = crate::jev::system_now()();
                self.log(LogRow {
                    at,
                    level: LogLevel::Error,
                    code: E_OBSERVER_PANIC,
                    job_id: None,
                    omp_version: Some(self.omp_version.clone()),
                    jev_model: None,
                    detail: Some("pass".to_owned()),
                });
                Pass::Waiting { until: None }
            }
        }
    }

    async fn pass(&self) -> Pass {
        let now = (self.now)();
        if !self.started.swap(true, Ordering::SeqCst) {
            self.scan_everything(now).await;
        }
        let expired = lock(&self.store).expire_memory_jobs(now, JOB_TTL_SECONDS, &self.omp_version);
        if let Err(err) = expired {
            tracing::warn!(error = %err, "memory jobs not expired");
        }
        self.maintain(now);
        let mut failed_here: HashSet<i64> = HashSet::new();
        loop {
            let pending = lock(&self.store).pending_memory_jobs(BATCH + failed_here.len());
            let jobs: Vec<MemoryJob> = match pending {
                Ok(pending) if !pending.unreadable.is_empty() => {
                    if !self.fail_unreadable(&pending.unreadable, now) {
                        return Pass::Waiting { until: None };
                    }
                    continue;
                }
                Ok(pending) => pending
                    .jobs
                    .into_iter()
                    .filter(|job| !failed_here.contains(&job.id))
                    .take(BATCH)
                    .collect(),
                Err(err) => {
                    tracing::warn!(error = %err, "memory jobs not readable");
                    return Pass::Waiting { until: None };
                }
            };
            if jobs.is_empty() {
                return Pass::Idle;
            }
            let mut finished: Vec<(MemoryJob, Work)> = Vec::new();
            let mut refused = None;
            for job in jobs {
                let processed = AssertUnwindSafe(self.process(&job, now))
                    .catch_unwind()
                    .await;
                match processed {
                    Ok(Ok(work)) => finished.push((job, work)),
                    Ok(Err(Failure::Jev(err))) => {
                        // A rejected request or an answer that does not fit may be this
                        // job's own fault: it counts one attempt, and a job given up no
                        // longer holds jev's gate for the jobs behind it.
                        if let Some(code) = job_fault(&err)
                            && self.fail_attempt(job.id, code, now)
                        {
                            self.jev.release_job();
                            self.pause_logged.store(false, Ordering::SeqCst);
                            failed_here.insert(job.id);
                            continue;
                        }
                        refused = Some(err);
                        break;
                    }
                    Ok(Err(Failure::Job(code))) => {
                        self.fail_attempt(job.id, code, now);
                        failed_here.insert(job.id);
                    }
                    Err(_) => {
                        tracing::error!(job = job.id, "memory job panicked");
                        self.fail_attempt(job.id, E_OBSERVER_PANIC, now);
                        failed_here.insert(job.id);
                    }
                }
            }
            if !finished.is_empty() && !self.write(finished, now).await {
                return Pass::Waiting { until: None };
            }
            if let Some(err) = refused {
                return Pass::Waiting {
                    until: self.jev_refused(&err, now),
                };
            }
        }
    }

    /// Settles pending rows whose kind or payload does not decode as `failed`, one
    /// [`E_OBSERVER_JOB`] row each: no later pass can read them either. False when the store
    /// could not settle them; the pass then waits rather than reading them again at once.
    fn fail_unreadable(&self, ids: &[i64], now: i64) -> bool {
        let settled: Vec<Settled> = ids
            .iter()
            .map(|&id| Settled {
                id,
                end: JobEnd::Failed,
                level: LogLevel::Error,
                code: E_OBSERVER_JOB,
                omp_version: self.omp_version.clone(),
                jev_model: None,
                detail: Some("unreadable".to_owned()),
            })
            .collect();
        tracing::warn!(jobs = ?ids, "unreadable memory jobs failed");
        match lock(&self.store).settle_memory_jobs(&settled, now) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(error = %err, "unreadable memory jobs not settled");
                false
            }
        }
    }

    /// Counts one failed attempt of job `id`; true when that was its last and it failed.
    fn fail_attempt(&self, id: i64, code: &'static str, now: i64) -> bool {
        let failed = lock(&self.store).fail_memory_job_attempt(id, code, &self.omp_version, now);
        match failed {
            Ok(true) => {
                tracing::warn!(job = id, code, "memory job failed");
                true
            }
            Ok(false) => false,
            Err(err) => {
                tracing::warn!(job = id, error = %err, "memory job attempt not counted");
                false
            }
        }
    }

    /// Logs a jev refusal (a pause once) and returns when the gate opens again.
    fn jev_refused(&self, err: &JevError, now: i64) -> Option<i64> {
        match err {
            JevError::NotYet { retry_at } => Some(*retry_at),
            JevError::Transient {
                code,
                status,
                retry_at,
            } => {
                tracing::warn!(code, ?status, retry_at, "jev call failed; memory jobs wait");
                Some(*retry_at)
            }
            JevError::Permanent { code, status } => {
                self.log_pause(code, *status, now);
                None
            }
            JevError::Paused { code } => {
                self.log_pause(code, None, now);
                None
            }
            JevError::Off => None,
        }
    }

    fn log_pause(&self, code: &'static str, status: Option<u16>, now: i64) {
        if self.pause_logged.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::error!(code, ?status, "jev paused until restart; memory jobs wait");
        self.log(LogRow {
            at: now,
            level: LogLevel::Error,
            code,
            job_id: None,
            omp_version: Some(self.omp_version.clone()),
            jev_model: None,
            detail: status.map(|s| format!("status={s}")),
        });
    }

    fn log(&self, row: LogRow) {
        if let Err(err) = lock(&self.store).memory_log(&row) {
            tracing::warn!(code = row.code, error = %err, "memory log row not written");
        }
    }

    /// Once per calendar day (UTC): prune old log rows and settled jobs, and warn about a large
    /// backlog.
    fn maintain(&self, now: i64) {
        let day = now.div_euclid(24 * 60 * 60);
        {
            let mut pruned = lock(&self.pruned_day);
            if *pruned == Some(day) {
                return;
            }
            *pruned = Some(day);
        }
        let mut store = lock(&self.store);
        if let Err(err) = store.prune_memory(now) {
            tracing::warn!(error = %err, "memory log not pruned");
        }
        let backlog = store.memory_backlog().unwrap_or_default();
        let pending = backlog.groups + backlog.other;
        drop(store);
        if pending > BACKLOG_WARN {
            tracing::warn!(pending, "memory backlog is large");
            self.log(LogRow {
                at: now,
                level: LogLevel::Warn,
                code: W_MEMORY_BACKLOG,
                job_id: None,
                omp_version: Some(self.omp_version.clone()),
                jev_model: None,
                detail: Some(format!("pending={pending}")),
            });
        }
    }

    /// Writes every entry of `finished` in one batch, then settles the jobs with their log
    /// rows. False when the memory writer has stopped or the jobs could not be settled: the
    /// pass then waits, so jev is not asked about the same jobs again at once.
    async fn write(&self, finished: Vec<(MemoryJob, Work)>, now: i64) -> bool {
        let mut ops = Vec::new();
        let mut job_of = Vec::new();
        for (index, (_, work)) in finished.iter().enumerate() {
            for entry in &work.entries {
                ops.push(Op::AddUnique(entry.clone()));
                job_of.push(index);
            }
        }
        let results = if ops.is_empty() {
            Vec::new()
        } else {
            match self.memory.apply_batch(ops).await {
                Ok(results) => results,
                Err(err) => {
                    tracing::error!(code = err.code, "memory writer stopped");
                    self.memory_stopped.store(true, Ordering::SeqCst);
                    return false;
                }
            }
        };
        let mut saved = vec![0usize; finished.len()];
        let mut skipped = vec![0usize; finished.len()];
        for (index, result) in job_of.into_iter().zip(results) {
            match result {
                Ok(Outcome::Added(added)) if added.created => saved[index] += 1,
                Ok(_) => skipped[index] += 1,
                Err(err) => {
                    skipped[index] += 1;
                    let job = finished[index].0.id;
                    tracing::warn!(job, code = err.code, "memory entry not written");
                    self.log(LogRow {
                        at: now,
                        level: LogLevel::Warn,
                        code: W_MEMORY_WRITE,
                        job_id: Some(job),
                        omp_version: Some(self.omp_version.clone()),
                        jev_model: None,
                        detail: Some(format!("error={}", err.code)),
                    });
                }
            }
        }
        let settled: Vec<Settled> = finished
            .iter()
            .enumerate()
            .map(|(index, (job, work))| Settled {
                id: job.id,
                end: JobEnd::Done,
                level: LogLevel::Info,
                code: work.code,
                omp_version: self.omp_version.clone(),
                jev_model: work.model.clone(),
                detail: Some(format!(
                    "kind={} saved={} skipped={} requests={} tokens={}",
                    job.kind.as_str(),
                    saved[index],
                    skipped[index],
                    work.requests,
                    work.tokens
                )),
            })
            .collect();
        let result = lock(&self.store).settle_memory_jobs(&settled, now);
        if let Err(err) = result {
            tracing::warn!(error = %err, "memory jobs not settled; they run again later");
            return false;
        }
        self.progress
            .send_modify(|count| *count += settled.len() as u64);
        true
    }

    async fn process(&self, job: &MemoryJob, now: i64) -> Result<Work, Failure> {
        let mut work = Work::new();
        let off = self.jev.status() == JevStatus::Off;
        match &job.payload {
            crate::store::JobPayload::Group(group) => {
                self.group(job, group, off, now, &mut work).await?;
            }
            crate::store::JobPayload::Lane(lane) => {
                self.lane(lane, off, now, &mut work).await?;
            }
            crate::store::JobPayload::Subagent(subagent) => {
                self.subagent(subagent, off, &mut work).await?;
            }
            crate::store::JobPayload::Scan(scan) => {
                self.scan_root(
                    &omp::lane_session_dir(&self.config, scan.lane),
                    &lane_session(scan.lane),
                    now,
                )
                .await?;
            }
        }
        if off && job.kind != JobKind::Scan && work.code == M_JOB_DONE {
            work.code = M_JEV_OFF;
        }
        Ok(work)
    }

    async fn group(
        &self,
        job: &MemoryJob,
        group: &GroupJob,
        off: bool,
        now: i64,
        work: &mut Work,
    ) -> Result<(), Failure> {
        self.scan_root(&omp::lead_session_dir(&self.config), "lead", now)
            .await?;
        // Decided from the cursor, not from this scan's count: an earlier job's scan in this
        // pass, or one that ran while this turn was still going, may have queued the file.
        if let Some(trace) = &group.trace
            && !job.partial
            && trace.task
            && trace.settled
            && !self.lead_artifact_written(group)?
        {
            self.layout_missing(job.id, now);
        }
        if off {
            return Ok(());
        }
        let tools: &[ToolUse] = group.trace.as_ref().map_or(&[], |t| &t.tools);
        let mut rows: Vec<i64> = group
            .rows
            .iter()
            .filter(|row| row.origin == "owner")
            .map(|row| row.id)
            .collect();
        rows.sort_unstable();
        for id in rows {
            let row = lock(&self.store).get(id)?;
            let Some(row) = row.filter(|row| row.origin == "owner") else {
                continue;
            };
            let candidates: Vec<(u32, String)> = memory::own_sentences(&row.text)
                .into_iter()
                .filter(|(_, s)| self.candidate(s))
                .collect();
            for window in candidates.chunks(self.window()).take(OWNER_WINDOWS) {
                let Some(pick) = self.judge(Kind::Owner, window, Some(tools), work).await? else {
                    continue;
                };
                let thread = thread_bucket(&row.conversation_key);
                let bucket = match pick.bucket.as_str() {
                    "thread" => thread,
                    "project" => {
                        match memory::mentioned_projects(self.memory.root(), &row.text).as_slice() {
                            [one] => Bucket::Project(one.clone()),
                            _ => thread,
                        }
                    }
                    // Owner decision 5, in code and not only in jev's option text: a sentence
                    // reaches core/owner.md only when it states a standing rule ("always",
                    // "from now on", ...), and core/boundaries.md only when it holds a whole
                    // always or never word (see `says_always`, `says_boundary`).
                    "core" if says_always(&pick.sentence) => Bucket::Owner,
                    "boundary" if says_boundary(&pick.sentence) => Bucket::Boundaries,
                    "core" | "boundary" => thread,
                    _ => continue,
                };
                // `pick` is a sentence of this window, cut from `row.text` above: the entry
                // is the owner's own sentence, verbatim.
                work.entries.push(entry(
                    pick.sentence,
                    Prov::Owner,
                    bucket,
                    Source::row_sentence(row.id, pick.n),
                ));
            }
        }
        Ok(())
    }

    async fn lane(
        &self,
        job: &LaneJob,
        off: bool,
        now: i64,
        work: &mut Work,
    ) -> Result<(), Failure> {
        self.scan_root(
            &omp::lane_session_dir(&self.config, job.lane),
            &lane_session(job.lane),
            now,
        )
        .await?;
        let row = lock(&self.store).get(job.row)?;
        let Some(row) = row.filter(|row| row.origin == "lane") else {
            return Ok(());
        };
        let plain = lead::parse_tag(&row.text)
            .is_some_and(|tag| tag.lane == Some(job.lane) && tag.failed.is_none() && !tag.stalled);
        let lane = lock(&self.store).lane(job.lane)?;
        let (true, Some(lane)) = (plain, lane) else {
            return Ok(());
        };
        let words = lead::lane_result_text(&row.text);
        if off || words.is_empty() || words == omp::EMPTY_REPLY {
            return Ok(());
        }
        let place = Place {
            thread: thread_bucket(&lane.conversation_key),
            project: self.lane_project(lane.repo.as_deref()),
        };
        self.excerpts(
            Kind::Lane,
            words,
            &place,
            Prov::Lane,
            Source::lane(job.lane, job.seq),
            work,
        )
        .await
    }

    async fn subagent(&self, job: &SubagentJob, off: bool, work: &mut Work) -> Result<(), Failure> {
        let (root, lane) = if job.session == "lead" {
            (omp::lead_session_dir(&self.config), None)
        } else if let Some(id) = job
            .session
            .strip_prefix("lane:")
            .and_then(|id| id.parse::<i64>().ok())
        {
            (omp::lane_session_dir(&self.config, id), Some(id))
        } else {
            return Err(Failure::Job(E_OBSERVER_JOB));
        };
        let path = Path::new(&job.path);
        if fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            work.code = M_ARTIFACT_CHANGED;
            return Ok(());
        }
        let usable = fs::canonicalize(&root)
            .is_ok_and(|root| artifact_problem(&root, path, &job.task, lane).is_ok());
        if !usable {
            work.code = W_SUBAGENT_SKIPPED;
            return Ok(());
        }
        let bytes = match read_artifact(path) {
            Ok(Some((bytes, _))) => bytes,
            Ok(None) => {
                work.code = W_SUBAGENT_SKIPPED;
                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                work.code = M_ARTIFACT_CHANGED;
                return Ok(());
            }
            Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
                work.code = W_SUBAGENT_SKIPPED;
                return Ok(());
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "subagent file not readable");
                return Err(Failure::Job(E_OBSERVER_JOB));
            }
        };
        if sha256_hex(&bytes) != job.sha256 {
            work.code = M_ARTIFACT_CHANGED;
            return Ok(());
        }
        if off {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&bytes);
        let (place, src) = match lane {
            Some(id) => {
                let lane = lock(&self.store).lane(id)?;
                let Some(lane) = lane else {
                    return Ok(());
                };
                (
                    Place {
                        thread: thread_bucket(&lane.conversation_key),
                        project: self.lane_project(lane.repo.as_deref()),
                    },
                    Source::new(&format!("lane={id} task={}", job.task)),
                )
            }
            None => (
                Place {
                    thread: Bucket::General,
                    project: Bucket::General,
                },
                Source::new(&format!("lead task={}", job.task)),
            ),
        };
        let Ok(src) = src else {
            work.code = W_SUBAGENT_SKIPPED;
            return Ok(());
        };
        let kind = Kind::Subagent {
            lane: lane.is_some(),
        };
        self.excerpts(kind, &text, &place, Prov::Subagent, src, work)
            .await
    }

    /// Judges a lane or subagent text: every line that starts with `[moraebot` is dropped
    /// ([`lead::lane_own_sentences`]), then every sentence the excerpt filter refuses, before
    /// anything is sent.
    async fn excerpts(
        &self,
        kind: Kind,
        text: &str,
        place: &Place,
        prov: Prov,
        src: Source,
        work: &mut Work,
    ) -> Result<(), Failure> {
        let candidates: Vec<(u32, String)> = lead::lane_own_sentences(text)
            .into_iter()
            .filter(|(_, s)| self.candidate(s) && self.filter.allows(s, &self.redactor).is_ok())
            .collect();
        for window in candidates.chunks(self.window()).take(EXCERPT_WINDOWS) {
            let Some(pick) = self.judge(kind, window, None, work).await? else {
                continue;
            };
            if self.filter.allows(&pick.sentence, &self.redactor).is_err() {
                continue;
            }
            let bucket = if pick.bucket == "thread" {
                place.thread.clone()
            } else {
                place.project.clone()
            };
            work.entries
                .push(entry(pick.sentence, prov, bucket, src.clone()));
        }
        Ok(())
    }

    fn window(&self) -> usize {
        (self.jev.config().window_sentences as usize).max(1)
    }

    /// A sentence that may be sent and saved: at least four characters, at most
    /// [`MAX_ENTRY_BYTES`], and nothing the redactor would change.
    fn candidate(&self, sentence: &str) -> bool {
        sentence.chars().count() >= MIN_SENTENCE_CHARS
            && sentence.len() <= MAX_ENTRY_BYTES
            && !self.redactor.redact(sentence).1
    }

    /// The project of a lane on checkout `repo`, when exactly one; else `projects/_general.md`.
    fn lane_project(&self, repo: Option<&str>) -> Bucket {
        let projects = repo.map_or_else(Vec::new, |repo| {
            memory::lane_projects(self.memory.root(), Path::new(repo))
        });
        match projects.as_slice() {
            [one] => Bucket::Project(one.clone()),
            _ => Bucket::General,
        }
    }

    /// Asks jev about one window; `None` when nothing in it is worth keeping.
    async fn judge(
        &self,
        kind: Kind,
        window: &[(u32, String)],
        tools: Option<&[ToolUse]>,
        work: &mut Work,
    ) -> Result<Option<Pick>, Failure> {
        let config = self.jev.config();
        let questions = &config.questions;
        let mut state = json!({
            "kind": kind.state_kind(),
            "sentences": window.iter().map(|(n, s)| format!("s{n}: {s}")).collect::<Vec<_>>(),
        });
        if let Some(tools) = tools {
            state["tools_used"] = serde_json::to_value(tools).unwrap_or(Value::Null);
        }
        let bucket_text = |name: &str| -> String {
            match name {
                "thread" => questions.bucket_thread.clone(),
                "project" => questions.bucket_project.clone(),
                "core" => questions.bucket_core.clone(),
                "boundary" => questions.bucket_boundary.clone(),
                _ => questions.bucket_none.clone(),
            }
        };
        let mut span: Vec<(String, String)> = window
            .iter()
            .map(|(n, _)| (format!("s{n}"), format!("Sentence s{n}.")))
            .collect();
        span.push(("none".into(), "No sentence is worth keeping.".into()));
        let request = Request {
            model: config.model.clone(),
            state,
            questions: vec![
                (
                    "save".into(),
                    Question::Noul {
                        instructions: questions.save.clone(),
                        yes: questions.save_yes.clone(),
                        no: questions.save_no.clone(),
                    },
                ),
                (
                    "bucket".into(),
                    Question::Choice {
                        instructions: questions.bucket.clone(),
                        options: kind
                            .buckets()
                            .iter()
                            .map(|name| ((*name).to_owned(), bucket_text(name)))
                            .collect(),
                    },
                ),
                (
                    "span".into(),
                    Question::Choice {
                        instructions: questions.span.clone(),
                        options: span,
                    },
                ),
            ],
        };
        let timeout = Duration::from_secs(config.timeout_seconds);
        let response = self
            .jev
            .ask(&request, timeout)
            .await
            .map_err(Failure::Jev)?;
        work.requests += 1;
        work.tokens += response.usage.input_tokens;
        work.model = Some(response.model.clone());
        let save = match response.answers.get("save") {
            Some(Answer::Noul { noul }) => *noul,
            _ => 0.0,
        };
        let choice = |name: &str| match response.answers.get(name) {
            Some(Answer::Choice { choice, .. }) => Some(choice.clone()),
            _ => None,
        };
        if save < config.save_threshold {
            return Ok(None);
        }
        let (Some(span), Some(bucket)) = (choice("span"), choice("bucket")) else {
            return Ok(None);
        };
        if kind == Kind::Owner && bucket == "none" {
            return Ok(None);
        }
        Ok(window
            .iter()
            .find(|(n, _)| span == format!("s{n}"))
            .map(|(n, sentence)| Pick {
                n: *n,
                sentence: sentence.clone(),
                bucket,
            }))
    }

    fn layout_missing(&self, job: i64, now: i64) {
        if self.layout_logged.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::warn!(
            code = E_SUBAGENT_LAYOUT,
            "a settled lead turn started a subagent but no subagent output file was found"
        );
        self.log(LogRow {
            at: now,
            level: LogLevel::Error,
            code: E_SUBAGENT_LAYOUT,
            job_id: Some(job),
            omp_version: Some(self.omp_version.clone()),
            jev_model: None,
            detail: Some(format!(
                "root={}",
                omp::lead_session_dir(&self.config).display()
            )),
        });
    }

    /// The start scan: the lead's session folder and every lane's.
    async fn scan_everything(&self, now: i64) {
        if let Err(err) = self
            .scan_root(&omp::lead_session_dir(&self.config), "lead", now)
            .await
        {
            tracing::warn!(error = %err, "lead session folder not scanned");
        }
        let Ok(lanes) = fs::read_dir(self.config.home.join("lanes")) else {
            return;
        };
        let ids: Vec<i64> = lanes
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i64>().ok())
            .collect();
        for id in ids {
            let root = omp::lane_session_dir(&self.config, id);
            if let Err(err) = self.scan_root(&root, &lane_session(id), now).await {
                tracing::warn!(lane = id, error = %err, "lane session folder not scanned");
            }
        }
    }

    /// Finds the subagent output files under session folder `root` (see
    /// [`artifact_problem`]) and queues each new or changed one as a `subagent` job of
    /// `session`. Returns how many were new or changed. The file work runs on a blocking
    /// thread, so a large session folder never holds a runtime worker.
    pub async fn scan_root(
        &self,
        root: &Path,
        session: &str,
        now: i64,
    ) -> Result<usize, StoreError> {
        let scanner = Scanner {
            store: Arc::clone(&self.store),
            omp_version: self.omp_version.clone(),
        };
        let (root, session) = (root.to_path_buf(), session.to_owned());
        match tokio::task::spawn_blocking(move || scanner.scan(&root, &session, now)).await {
            Ok(found) => found,
            // A panic in the scan stays a panic of this job (caught in `run_once`).
            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
            Err(_) => Ok(0),
        }
    }

    /// Whether the store knows a subagent output file under the lead's session folder that
    /// was written during the turn of `group` (since its first row arrived), whichever scan
    /// queued it.
    fn lead_artifact_written(&self, group: &GroupJob) -> Result<bool, StoreError> {
        let Ok(root) = fs::canonicalize(omp::lead_session_dir(&self.config)) else {
            return Ok(false);
        };
        let Some(first) = group.rows.iter().map(|row| row.id).min() else {
            return Ok(false);
        };
        let prefix = format!("{}/", root.to_string_lossy());
        lock(&self.store).artifact_written_since(&prefix, first)
    }
}

/// The subagent output scan, apart from the observer so it can run on a blocking thread.
struct Scanner {
    store: Arc<Mutex<Store>>,
    omp_version: String,
}

impl Scanner {
    fn scan(&self, root: &Path, session: &str, now: i64) -> Result<usize, StoreError> {
        let Ok(root) = fs::canonicalize(root) else {
            return Ok(0);
        };
        let lane = session
            .strip_prefix("lane:")
            .and_then(|id| id.parse::<i64>().ok());
        let mut found = 0;
        let mut folders = vec![(root.clone(), 0usize)];
        while let Some((folder, depth)) = folders.pop() {
            let Ok(entries) = fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
                if !is_dir {
                    continue;
                }
                if regular(&sibling(&path, "jsonl")) {
                    found += self.scan_artifacts(&root, &path, session, lane, now)?;
                }
                if depth + 1 < MAX_SCAN_DEPTH {
                    folders.push((path, depth + 1));
                }
            }
        }
        Ok(found)
    }

    /// Queues the `<task>.md` files of artifact folder `folder` that have a `<task>.jsonl`. A
    /// file whose size and mtime match its cursor row is not read again.
    fn scan_artifacts(
        &self,
        root: &Path,
        folder: &Path,
        session: &str,
        lane: Option<i64>,
        now: i64,
    ) -> Result<usize, StoreError> {
        let Ok(entries) = fs::read_dir(folder) else {
            return Ok(0);
        };
        let mut found = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(task) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".md"))
                .map(str::to_owned)
            else {
                continue;
            };
            if !regular(&folder.join(format!("{task}.jsonl"))) {
                continue;
            }
            if let Err(reason) = artifact_problem(root, &path, &task, lane) {
                self.skipped(&path, reason, now)?;
                continue;
            }
            let shown = path.to_string_lossy().into_owned();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if lock(&self.store).artifact_unchanged(&shown, stamp_size(&meta), mtime_ns(&meta))? {
                continue;
            }
            let (bytes, meta) = match read_artifact(&path) {
                Ok(Some(read)) => read,
                Ok(None) => {
                    self.skipped(&path, "not a regular file or too large when opened", now)?;
                    continue;
                }
                Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
                    self.skipped(&path, "symlink", now)?;
                    continue;
                }
                Err(_) => continue,
            };
            let job = SubagentJob {
                path: shown,
                session: session.to_owned(),
                task,
                sha256: sha256_hex(&bytes),
            };
            if lock(&self.store).note_artifact(&job, stamp_size(&meta), mtime_ns(&meta), now)? {
                found += 1;
            }
        }
        Ok(found)
    }

    /// One [`W_SUBAGENT_SKIPPED`] row per path.
    fn skipped(&self, path: &Path, reason: &str, now: i64) -> Result<(), StoreError> {
        let detail = path.to_string_lossy().into_owned();
        let store = lock(&self.store);
        if store.memory_logged(W_SUBAGENT_SKIPPED, &detail)? {
            return Ok(());
        }
        tracing::warn!(path = %detail, reason, "subagent file skipped");
        store.memory_log(&LogRow {
            at: now,
            level: LogLevel::Warn,
            code: W_SUBAGENT_SKIPPED,
            job_id: None,
            omp_version: Some(self.omp_version.clone()),
            jev_model: None,
            detail: Some(detail),
        })
    }
}

/// The code of a jev failure that may come from the job's own request (a rejected request, an
/// answer that does not fit), so it counts one attempt of that job. Nothing was sent for the
/// other refusals, or the fault is not the job's.
fn job_fault(err: &JevError) -> Option<&'static str> {
    match err {
        JevError::Permanent {
            code: E_JEV_REQUEST,
            ..
        } => Some(E_JEV_REQUEST),
        JevError::Transient {
            code: E_JEV_RESPONSE,
            ..
        } => Some(E_JEV_RESPONSE),
        _ => None,
    }
}

/// Where a lane or subagent entry goes for jev's `thread` and every other answer.
struct Place {
    thread: Bucket,
    project: Bucket,
}

fn entry(text: String, prov: Prov, bucket: Bucket, src: Source) -> NewEntry {
    NewEntry {
        text,
        prov,
        bucket,
        src,
        expires: None,
        why: Some(WHY.to_owned()),
    }
}

/// The conversation's bucket, or `projects/_general.md` when its key is not a usable file name.
fn thread_bucket(key: &str) -> Bucket {
    let thread = Bucket::Thread(key.to_owned());
    if thread.validate().is_ok() {
        thread
    } else {
        Bucket::General
    }
}

fn lane_session(id: i64) -> String {
    format!("lane:{id}")
}

/// `<parent>/<name>.<extension>` for `path` = `<parent>/<name>` (the name may hold dots).
fn sibling(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(extension);
    path.with_file_name(name)
}

fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// Why `path` is not a usable subagent output file of a session under canonical `root`:
/// a symlink or not a regular file, larger than [`MAX_ARTIFACT_BYTES`], outside `root`, or a
/// `task` that is not usable in a source. A lane runs as the same user and could plant a link
/// to any file; it is never followed.
fn artifact_problem(
    root: &Path,
    path: &Path,
    task: &str,
    lane: Option<i64>,
) -> Result<(), &'static str> {
    let meta = fs::symlink_metadata(path).map_err(|_| "missing")?;
    if meta.file_type().is_symlink() {
        return Err("symlink");
    }
    if !meta.is_file() {
        return Err("not a regular file");
    }
    if meta.len() > MAX_ARTIFACT_BYTES {
        return Err("too large");
    }
    let inside = fs::canonicalize(path).is_ok_and(|real| real.starts_with(root));
    if !inside {
        return Err("outside the session folder");
    }
    let src = match lane {
        Some(id) => format!("lane={id} task={task}"),
        None => format!("lead task={task}"),
    };
    if Source::new(&src).is_err() {
        return Err("task name");
    }
    Ok(())
}

/// Reads subagent output `path` through one handle opened without following a symlink and
/// without blocking on a FIFO, checking the opened file itself: `Ok(None)` when it is not a
/// regular file or is larger than [`MAX_ARTIFACT_BYTES`]. A file swapped after
/// [`artifact_problem`] checked it is so never followed, and never read without end. A
/// symlink fails with `ELOOP`.
fn read_artifact(path: &Path) -> std::io::Result<Option<(Vec<u8>, fs::Metadata)>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_ARTIFACT_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_ARTIFACT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Ok(None);
    }
    Ok(Some((bytes, meta)))
}

fn stamp_size(meta: &fs::Metadata) -> i64 {
    i64::try_from(meta.len()).unwrap_or(i64::MAX)
}

/// The modification time in nanoseconds since the epoch, as the cursor stores it.
fn mtime_ns(meta: &fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.mtime_nsec())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
