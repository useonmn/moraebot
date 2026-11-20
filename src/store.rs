//! Durable SQLite inbox: store-first admission, global FIFO, restart recovery.
//!
//! One connection, `locking_mode=EXCLUSIVE` (single instance), WAL, `synchronous=FULL`.
//! Because of the exclusive lock, nothing else may open the file while a `Store` is
//! alive: read everything through this API.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Row as SqlRow, Transaction, TransactionBehavior,
    params,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::files::E_FILE_INCOMPLETE;
use crate::lane::{E_LANE_INTERRUPTED, LANE_PLATFORM};

pub const E_ALREADY_RUNNING: &str = "E_ALREADY_RUNNING";
pub const E_STORE_WAL: &str = "E_STORE_WAL";
pub const E_STORE_INVALID: &str = "E_STORE_INVALID";
pub const E_STORE: &str = "E_STORE";
/// Stored row error: held behind an interrupted message of the same conversation.
pub const E_DISPATCH_HELD: &str = "E_DISPATCH_HELD";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{E_ALREADY_RUNNING}: another moraebot instance holds the inbox database")]
    AlreadyRunning,
    #[error("{E_STORE_WAL}: could not enable WAL journal mode (got '{0}')")]
    WalUnavailable(String),
    #[error("{E_STORE_INVALID}: {0}")]
    Invalid(&'static str),
    #[error("{E_STORE}: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{E_STORE}: {0}")]
    Io(#[from] std::io::Error),
}

impl StoreError {
    /// Stable code for this error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::AlreadyRunning => E_ALREADY_RUNNING,
            Self::WalUnavailable(_) => E_STORE_WAL,
            Self::Invalid(_) => E_STORE_INVALID,
            Self::Sqlite(_) | Self::Io(_) => E_STORE,
        }
    }
}

/// Maps lock contention to the already-running error, everything else to `Sqlite`.
fn classify(err: rusqlite::Error) -> StoreError {
    match err.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => StoreError::AlreadyRunning,
        _ => StoreError::Sqlite(err),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Queued,
    Processing,
    ReplyPending,
    Done,
    Failed,
    NeedsCheck,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::ReplyPending => "reply_pending",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::NeedsCheck => "needs_check",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "processing" => Self::Processing,
            "reply_pending" => Self::ReplyPending,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "needs_check" => Self::NeedsCheck,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRow {
    pub id: i64,
    pub platform: String,
    pub message_id: String,
    pub conversation_key: String,
    pub author_id: String,
    pub text: String,
    pub state: State,
    /// The reply text.
    pub reply: Option<String>,
    pub notice_sent: bool,
    pub error: Option<String>,
    /// Attachment JSON: declared metadata at admission, saved files once fetched.
    pub attachments: Option<String>,
    /// False while the row's attachments are still being fetched.
    pub files_ready: bool,
    /// Outbound-file JSON stored with the reply.
    pub out_files: Option<String>,
    /// The reply text was fully delivered.
    pub text_sent: bool,
    /// `owner` for a messenger message, `lane` for a lane result.
    pub origin: String,
    /// The main row whose lead turn this row was steered into.
    pub steered_into: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admitted {
    New(i64),
    Duplicate,
}

/// Result of [`Store::recover_on_start`].
#[derive(Debug, Default)]
pub struct Recovered {
    /// Rows that were `processing` and are now `needs_check` (never re-queued).
    pub needs_check: Vec<InboxRow>,
    /// Rows holding a stored reply that still has to be (re)sent.
    pub reply_pending: Vec<InboxRow>,
    /// `reply_pending` rows with an outbound file whose upload was cut; the file is now skipped.
    pub interrupted_uploads: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    Running,
    Idle,
    Failed,
    NeedsCheck,
}

impl LaneState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Failed => "failed",
            Self::NeedsCheck => "needs_check",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => Self::Running,
            "idle" => Self::Idle,
            "failed" => Self::Failed,
            "needs_check" => Self::NeedsCheck,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRow {
    pub id: i64,
    pub conversation_key: String,
    pub task: String,
    pub state: LaneState,
    pub error: Option<String>,
    /// Prompts sent to the lane so far; the newest one's sequence number.
    pub prompts: i64,
    pub omp_session_id: Option<String>,
    pub notice_sent: bool,
    /// The canonical Done line the lane started with; `None` for a lane started before slice 5.
    pub done: Option<String>,
    /// The canonical git checkout the lane's worktree comes from; `None` = the shared work folder.
    pub repo: Option<String>,
    /// Set once by `lane_close`; a closed lane takes no more prompts.
    pub closed: bool,
    /// The reason the lead gave when it first closed the lane.
    pub close_reason: Option<String>,
}

/// Result of [`Store::start_lane`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneStart {
    /// A new running lane with this id.
    New(i64),
    /// The tool call already started this lane.
    Existing(i64),
    /// The running-lane cap is reached; nothing was stored.
    Limit,
}

/// Result of [`Store::message_lane`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneMessage {
    /// The lane runs again; the new prompt's sequence number.
    Seq(i64),
    Unknown,
    Busy,
    Limit,
    /// The lane was closed with `lane_close`.
    Closed,
}

/// Result of [`Store::close_lane`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneClose {
    /// The lane is closed (now or before); its canonical repo, if any.
    Closed {
        repo: Option<String>,
    },
    Unknown,
    /// The lane is running; nothing changed.
    Busy,
}

/// Where the first meeting with the owner stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingState {
    Active,
    Done,
    Skipped,
}

impl OnboardingState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Done => "done",
            Self::Skipped => "skipped",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "done" => Some(Self::Done),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// The onboarding row: its state and the newest inbox row id when it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingRow {
    pub state: OnboardingState,
    pub after_row: i64,
}

pub struct Store {
    conn: Connection,
    /// Called after every commit that inserted a memory job ([`Store::set_memory_wake`]).
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Dropped after `conn` (fields drop in order), so [`StoreClosed`] resolves only once the
    /// connection and its exclusive lock are gone.
    closed: Option<mpsc::Sender<()>>,
}

/// Resolves when the [`Store`] it came from was dropped and released the database.
pub struct StoreClosed(mpsc::Receiver<()>);

impl StoreClosed {
    /// Blocks until the store was dropped, at most `limit`. Returns `false` when it is still
    /// open after `limit`.
    pub fn wait(self, limit: Duration) -> bool {
        !matches!(
            self.0.recv_timeout(limit),
            Err(mpsc::RecvTimeoutError::Timeout)
        )
    }
}

/// Most distinct tools one [`TurnTrace`] keeps.
pub const MAX_TRACE_TOOLS: usize = 64;
/// Longest tool name a [`ToolUse`] keeps, in characters.
const MAX_TOOL_NAME: usize = 64;
/// Processing errors (never jev's) after which a memory job is `failed`.
pub const MAX_JOB_ATTEMPTS: i64 = 3;
/// How long a memory log row or a settled memory job is kept, in seconds (30 days).
const MEMORY_KEEP_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Log code of a pending job that expired before it was processed.
pub const M_JOB_EXPIRED: &str = "M_JOB_EXPIRED";

/// What a tool the lead started may read or touch ([`crate::memory_tools::tool_source`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolSource {
    Web,
    Github,
    Fs,
    Shell,
    Host,
    Task,
    Other,
}

/// One distinct tool of a [`TurnTrace`]: its cleaned name and source, never its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUse {
    pub name: String,
    pub source: ToolSource,
}

impl ToolUse {
    /// `name` lowercased, cut to [`MAX_TOOL_NAME`] characters, and every character outside
    /// `[a-z0-9_.:-]` replaced by `_`.
    pub fn new(name: &str, source: ToolSource) -> Self {
        let name = name
            .to_lowercase()
            .chars()
            .take(MAX_TOOL_NAME)
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() || "_.:-".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Self { name, source }
    }
}

/// The content-free record of one lead turn group: which tools ran, never what they read,
/// were given or answered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTrace {
    /// Distinct tools the lead started in this group, in first-seen order, at most
    /// [`MAX_TRACE_TOOLS`].
    pub tools: Vec<ToolUse>,
    /// The group started the `task` tool (a subagent).
    pub task: bool,
    /// omp reported the session settled (no background job pending). As in the lead's
    /// restart check, a turn that ended in an error counts as settled, so this is only
    /// meaningful for a group that is not `partial`.
    pub settled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Group,
    Lane,
    Subagent,
    Scan,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Lane => "lane",
            Self::Subagent => "subagent",
            Self::Scan => "scan",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "group" => Self::Group,
            "lane" => Self::Lane,
            "subagent" => Self::Subagent,
            "scan" => Self::Scan,
            _ => return None,
        })
    }
}

/// The state a memory job leaves `pending` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobEnd {
    Done,
    Failed,
    Expired,
}

impl JobEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One inbox row of a `group` job and its origin, read from the row itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRow {
    pub id: i64,
    pub origin: String,
}

/// A settled, failed, interrupted or restart-cut lead turn group: its rows (main row first)
/// and its trace (`None` only for a group cut by a restart).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupJob {
    pub rows: Vec<JobRow>,
    pub trace: Option<TurnTrace>,
}

/// A lane result: `row` is the inbox row that holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneJob {
    pub row: i64,
    pub lane: i64,
    pub seq: i64,
}

/// A new or changed subagent output file. `session` is `lead` or `lane:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentJob {
    pub path: String,
    pub session: String,
    pub task: String,
    pub sha256: String,
}

/// The session folder of a closed lane, to scan once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanJob {
    pub lane: i64,
}

/// The payload of a memory job, one shape per [`JobKind`]. It never holds message text,
/// lane text, a reply or tool arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum JobPayload {
    Group(GroupJob),
    Lane(LaneJob),
    Subagent(SubagentJob),
    Scan(ScanJob),
}

impl JobPayload {
    pub fn kind(&self) -> JobKind {
        match self {
            Self::Group(_) => JobKind::Group,
            Self::Lane(_) => JobKind::Lane,
            Self::Subagent(_) => JobKind::Subagent,
            Self::Scan(_) => JobKind::Scan,
        }
    }

    /// Reads a stored payload by its job's kind (the JSON carries no tag).
    fn parse(kind: JobKind, json: &str) -> serde_json::Result<Self> {
        Ok(match kind {
            JobKind::Group => Self::Group(serde_json::from_str(json)?),
            JobKind::Lane => Self::Lane(serde_json::from_str(json)?),
            JobKind::Subagent => Self::Subagent(serde_json::from_str(json)?),
            JobKind::Scan => Self::Scan(serde_json::from_str(json)?),
        })
    }
}

/// What [`Store::pending_memory_jobs`] read: the jobs, and the ids of pending rows whose kind or
/// payload does not decode.
#[derive(Debug, Default)]
pub struct PendingJobs {
    pub jobs: Vec<MemoryJob>,
    pub unreadable: Vec<i64>,
}

/// A pending memory job ([`Store::pending_memory_jobs`]). Times are unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryJob {
    pub id: i64,
    pub kind: JobKind,
    /// The group failed, was interrupted, or was cut by a restart.
    pub partial: bool,
    pub payload: JobPayload,
    pub attempts: i64,
    pub created_at: i64,
}

/// How one job leaves `pending` ([`Store::settle_memory_jobs`]): its end state and its
/// log row. `code` is a stable code; `detail` holds ids, counts and paths, never entry text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub id: i64,
    pub end: JobEnd,
    pub level: LogLevel,
    pub code: &'static str,
    pub omp_version: String,
    pub jev_model: Option<String>,
    pub detail: Option<String>,
}

/// One `memory_log` row ([`Store::memory_log`]). `at` is unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub at: i64,
    pub level: LogLevel,
    pub code: &'static str,
    pub job_id: Option<i64>,
    pub omp_version: Option<String>,
    pub jev_model: Option<String>,
    pub detail: Option<String>,
}

/// Pending memory jobs ([`Store::memory_backlog`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backlog {
    pub groups: usize,
    pub other: usize,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS inbox(
    id INTEGER PRIMARY KEY,
    platform TEXT NOT NULL,
    message_id TEXT NOT NULL,
    conversation_key TEXT NOT NULL,
    author_id TEXT NOT NULL,
    text TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('queued','processing','reply_pending','done','failed','needs_check')),
    reply TEXT,
    notice_sent INTEGER NOT NULL DEFAULT 0,
    received_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    error TEXT,
    attachments TEXT,
    files_ready INTEGER NOT NULL DEFAULT 1,
    out_files TEXT,
    text_sent INTEGER NOT NULL DEFAULT 0,
    origin TEXT NOT NULL DEFAULT 'owner',
    steered_into INTEGER,
    memory_answered INTEGER NOT NULL DEFAULT 0,
    UNIQUE(platform, message_id)
);
CREATE TABLE IF NOT EXISTS conversations(
    conversation_key TEXT PRIMARY KEY,
    platform TEXT,
    channel_id TEXT NOT NULL,
    omp_session_id TEXT,
    created_at TEXT,
    last_activity_at TEXT
);
CREATE TABLE IF NOT EXISTS known_threads(
    platform TEXT,
    thread_id TEXT,
    PRIMARY KEY(platform, thread_id)
);
CREATE TABLE IF NOT EXISTS sessions(
    name TEXT PRIMARY KEY,
    omp_session_id TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS lanes(
    id INTEGER PRIMARY KEY,
    tool_call_id TEXT NOT NULL UNIQUE,
    conversation_key TEXT NOT NULL,
    task TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('running','idle','failed','needs_check')),
    error TEXT,
    prompts INTEGER NOT NULL DEFAULT 1,
    omp_session_id TEXT,
    notice_sent INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    done TEXT,
    repo TEXT,
    closed_at TEXT,
    close_reason TEXT
);
CREATE TABLE IF NOT EXISTS onboarding(
    id INTEGER PRIMARY KEY CHECK(id = 1),
    state TEXT NOT NULL CHECK(state IN ('active','done','skipped')),
    after_row INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT
);
CREATE TABLE IF NOT EXISTS memory_jobs(
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('group','lane','subagent','scan')),
    key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','done','failed','expired')),
    partial INTEGER NOT NULL DEFAULT 0,
    payload TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(kind, key)
);
CREATE INDEX IF NOT EXISTS memory_jobs_pending ON memory_jobs(state, id);
CREATE TABLE IF NOT EXISTS memory_log(
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    level TEXT NOT NULL CHECK(level IN ('info','warn','error')),
    code TEXT NOT NULL,
    job_id INTEGER,
    omp_version TEXT,
    jev_model TEXT,
    detail TEXT
);
CREATE TABLE IF NOT EXISTS memory_cursor(
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    seen_at INTEGER NOT NULL
);
";

/// Tables an older version created and nothing reads any more; `Store::open` drops them.
/// `memory_notices` held the `[memory] saved ...` lines once posted under replies (#56).
const DROPPED_TABLES: [&str; 1] = ["memory_notices"];

/// Columns added after slice 2 as `(table, column, declaration)`; `Store::open` adds the
/// missing ones to an older database.
const ADDED_COLUMNS: [(&str, &str, &str); 11] = [
    ("inbox", "attachments", "TEXT"),
    ("inbox", "files_ready", "INTEGER NOT NULL DEFAULT 1"),
    ("inbox", "out_files", "TEXT"),
    ("inbox", "text_sent", "INTEGER NOT NULL DEFAULT 0"),
    ("inbox", "origin", "TEXT NOT NULL DEFAULT 'owner'"),
    ("inbox", "steered_into", "INTEGER"),
    ("inbox", "memory_answered", "INTEGER NOT NULL DEFAULT 0"),
    ("lanes", "done", "TEXT"),
    ("lanes", "repo", "TEXT"),
    ("lanes", "closed_at", "TEXT"),
    ("lanes", "close_reason", "TEXT"),
];

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";
/// The current time in unix seconds, for the `memory_*` tables.
const NOW_UNIX: &str = "CAST(strftime('%s','now') AS INTEGER)";

fn insert_log(conn: &Connection, row: &LogRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_log(at, level, code, job_id, omp_version, jev_model, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            row.at,
            row.level.as_str(),
            row.code,
            row.job_id,
            row.omp_version,
            row.jev_model,
            row.detail
        ],
    )?;
    Ok(())
}

/// Inserts one pending memory job; an existing `(kind, key)` changes nothing. Returns whether
/// a row was inserted. `created_at` is `now`, or the database's clock when `None`.
fn insert_job(
    tx: &Transaction,
    key: &str,
    partial: bool,
    payload: &JobPayload,
    now: Option<i64>,
) -> rusqlite::Result<bool> {
    let json = serde_json::to_string(payload)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let at = now.map_or_else(|| NOW_UNIX.to_owned(), |now| now.to_string());
    Ok(tx.execute(
        &format!(
            "INSERT INTO memory_jobs(kind, key, partial, payload, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, {at}, {at})
             ON CONFLICT(kind, key) DO NOTHING"
        ),
        params![payload.kind().as_str(), key, partial, json],
    )? != 0)
}

/// Inserts the `group` job for `ids` (main row first), keyed by the main row. Origins are
/// read here, from the inbox rows, in the caller's transaction, so no caller can pass a
/// wrong one.
fn enqueue_group(
    tx: &Transaction,
    ids: &[i64],
    partial: bool,
    trace: Option<&TurnTrace>,
) -> rusqlite::Result<bool> {
    let Some(main) = ids.first() else {
        return Ok(false);
    };
    let mut rows = Vec::with_capacity(ids.len());
    for &id in ids {
        let origin: String =
            tx.query_row("SELECT origin FROM inbox WHERE id = ?1", [id], |r| r.get(0))?;
        rows.push(JobRow { id, origin });
    }
    let payload = JobPayload::Group(GroupJob {
        rows,
        trace: trace.cloned(),
    });
    insert_job(tx, &main.to_string(), partial, &payload, None)
}

/// Adds every missing column of [`ADDED_COLUMNS`] and drops every [`DROPPED_TABLES`] table,
/// in one transaction.
fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for table in DROPPED_TABLES {
        tx.execute_batch(&format!("DROP TABLE IF EXISTS {table}"))?;
    }
    for (table, name, decl) in ADDED_COLUMNS {
        let present: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            [table, name],
            |r| r.get(0),
        )?;
        if !present {
            tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {decl}"))?;
        }
    }
    tx.commit()
}

/// Updates one entry of a row's `out_files` JSON array in one statement. `set` is a
/// `json_set` argument list over `$e` (the entry path); fails when the entry is missing.
fn update_out_file(conn: &Connection, id: i64, index: usize, set: &str) -> Result<(), StoreError> {
    let entry = "'$[' || ?2 || ']'";
    let set = set.replace("$e", entry);
    let changed = conn.execute(
        &format!(
            "UPDATE inbox SET out_files = json_set(out_files, {set}), updated_at = {NOW}
             WHERE id = ?1 AND json_type(out_files, {entry}) = 'object'"
        ),
        params![id, index as i64],
    )?;
    if changed == 0 {
        return Err(StoreError::Invalid("no such outbound file"));
    }
    Ok(())
}

/// Skips every out-file entry still marked `attempting` and neither sent nor skipped.
/// Returns the rewritten JSON when anything changed.
fn skip_interrupted(out_files: &str) -> Option<String> {
    let mut files: Value = serde_json::from_str(out_files).ok()?;
    let mut changed = false;
    for entry in files.as_array_mut()?.iter_mut() {
        let flag = |k: &str| entry.get(k).and_then(Value::as_bool).unwrap_or(false);
        if flag("attempting") && !flag("sent") && !flag("skipped") {
            entry["skipped"] = Value::Bool(true);
            entry["attempting"] = Value::Bool(false);
            changed = true;
        }
    }
    changed.then(|| files.to_string())
}

/// Holds the queued owner rows of the conversations selected by the appended subquery:
/// they become `needs_check` / [`E_DISPATCH_HELD`] with their notice still to be posted.
/// Lane-result rows are never held: they answer no owner message.
fn hold_queued_sql(conversations: &str) -> String {
    format!(
        "UPDATE inbox SET state = 'needs_check', error = '{E_DISPATCH_HELD}', updated_at = {NOW}
         WHERE state = 'queued' AND origin = 'owner' AND conversation_key IN ({conversations})"
    )
}
const COLS: &str = "id, platform, message_id, conversation_key, author_id, text, state, reply, notice_sent, error, attachments, files_ready, out_files, text_sent, origin, steered_into";

const LANE_COLS: &str = "id, conversation_key, task, state, error, prompts, omp_session_id, notice_sent, done, repo, closed_at, close_reason";

fn map_lane(r: &SqlRow<'_>) -> rusqlite::Result<LaneRow> {
    let state: String = r.get(3)?;
    Ok(LaneRow {
        id: r.get(0)?,
        conversation_key: r.get(1)?,
        task: r.get(2)?,
        state: LaneState::parse(&state).ok_or(rusqlite::Error::InvalidColumnType(
            3,
            "state".into(),
            rusqlite::types::Type::Text,
        ))?,
        error: r.get(4)?,
        prompts: r.get(5)?,
        omp_session_id: r.get(6)?,
        notice_sent: r.get::<_, i64>(7)? != 0,
        done: r.get(8)?,
        repo: r.get(9)?,
        closed: r.get::<_, Option<String>>(10)?.is_some(),
        close_reason: r.get(11)?,
    })
}

fn running_lanes(conn: &Connection) -> rusqlite::Result<usize> {
    conn.query_row(
        "SELECT COUNT(*) FROM lanes WHERE state = 'running'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n as usize)
}

fn map_row(r: &SqlRow<'_>) -> rusqlite::Result<InboxRow> {
    let state: String = r.get(6)?;
    Ok(InboxRow {
        id: r.get(0)?,
        platform: r.get(1)?,
        message_id: r.get(2)?,
        conversation_key: r.get(3)?,
        author_id: r.get(4)?,
        text: r.get(5)?,
        state: State::parse(&state).ok_or(rusqlite::Error::InvalidColumnType(
            6,
            "state".into(),
            rusqlite::types::Type::Text,
        ))?,
        reply: r.get(7)?,
        notice_sent: r.get::<_, i64>(8)? != 0,
        error: r.get(9)?,
        attachments: r.get(10)?,
        files_ready: r.get::<_, i64>(11)? != 0,
        out_files: r.get(12)?,
        text_sent: r.get::<_, i64>(13)? != 0,
        origin: r.get(14)?,
        steered_into: r.get(15)?,
    })
}

impl Store {
    /// Opens (creating if needed) the inbox. Fails with [`StoreError::AlreadyRunning`]
    /// when another process holds the database.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        // Fail fast on a held lock instead of waiting (rusqlite defaults to 5 s).
        conn.busy_timeout(Duration::ZERO)?;
        // Owner-only; WAL files inherit the main file's mode.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        conn.pragma_update_and_check(None, "locking_mode", "EXCLUSIVE", |r| r.get::<_, String>(0))
            .map_err(classify)?;
        let mode = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))
            .map_err(classify)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::WalUnavailable(mode));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        // Writing the schema takes the exclusive lock, so a second instance fails here at the latest.
        conn.execute_batch(SCHEMA).map_err(classify)?;
        let mut conn = conn;
        migrate(&mut conn).map_err(classify)?;
        Ok(Self {
            conn,
            wake: None,
            closed: None,
        })
    }

    /// Sets the call made after every commit that inserted a memory job (the observer's wake).
    pub fn set_memory_wake(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.wake = Some(wake);
    }

    /// A [`StoreClosed`] that resolves once this store is dropped; a later call replaces it.
    pub fn closed(&mut self) -> StoreClosed {
        let (tx, rx) = mpsc::channel();
        self.closed = Some(tx);
        StoreClosed(rx)
    }

    /// Calls the memory wake when a job was inserted; call only after the commit.
    fn wake_after(&self, inserted: bool) {
        if inserted && let Some(wake) = &self.wake {
            wake();
        }
    }

    /// Up to `limit` pending memory jobs, oldest first. A row whose kind or payload does not
    /// decode is returned by id in `unreadable`, so one bad row never hides the others.
    pub fn pending_memory_jobs(&self, limit: usize) -> Result<PendingJobs, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, partial, payload, attempts, created_at FROM memory_jobs
             WHERE state = 'pending' ORDER BY id LIMIT ?1",
        )?;
        let rows: Vec<(i64, String, bool, Option<String>, i64, i64)> = stmt
            .query_map([limit as i64], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let mut pending = PendingJobs::default();
        for (id, kind, partial, payload, attempts, created_at) in rows {
            let decoded = JobKind::parse(&kind).and_then(|kind| {
                let payload = JobPayload::parse(kind, payload.as_deref()?).ok()?;
                Some((kind, payload))
            });
            match decoded {
                Some((kind, payload)) => pending.jobs.push(MemoryJob {
                    id,
                    kind,
                    partial,
                    payload,
                    attempts,
                    created_at,
                }),
                None => pending.unreadable.push(id),
            }
        }
        Ok(pending)
    }

    /// Moves each job out of `pending` (payload cleared) and writes its log row, in one
    /// transaction. A job that is no longer pending is left as it is and gets no row.
    pub fn settle_memory_jobs(&mut self, settled: &[Settled], now: i64) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for job in settled {
            let changed = tx.execute(
                "UPDATE memory_jobs SET state = ?2, payload = NULL, updated_at = ?3
                 WHERE id = ?1 AND state = 'pending'",
                params![job.id, job.end.as_str(), now],
            )?;
            if changed == 0 {
                continue;
            }
            insert_log(
                &tx,
                &LogRow {
                    at: now,
                    level: job.level,
                    code: job.code,
                    job_id: Some(job.id),
                    omp_version: Some(job.omp_version.clone()),
                    jev_model: job.jev_model.clone(),
                    detail: job.detail.clone(),
                },
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Counts one processing error of pending job `id` (stored as its `last_error`). At
    /// [`MAX_JOB_ATTEMPTS`] the job becomes `failed`, its payload is cleared and one error log
    /// row with `code` is written; then returns true.
    pub fn fail_memory_job_attempt(
        &mut self,
        id: i64,
        code: &'static str,
        omp_version: &str,
        now: i64,
    ) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempts: Option<i64> = tx
            .query_row(
                "UPDATE memory_jobs SET attempts = attempts + 1, last_error = ?2, updated_at = ?3
                 WHERE id = ?1 AND state = 'pending' RETURNING attempts",
                params![id, code, now],
                |r| r.get(0),
            )
            .optional()?;
        let Some(attempts) = attempts else {
            return Ok(false);
        };
        let failed = attempts >= MAX_JOB_ATTEMPTS;
        if failed {
            tx.execute(
                "UPDATE memory_jobs SET state = 'failed', payload = NULL WHERE id = ?1",
                [id],
            )?;
            insert_log(
                &tx,
                &LogRow {
                    at: now,
                    level: LogLevel::Error,
                    code,
                    job_id: Some(id),
                    omp_version: Some(omp_version.to_owned()),
                    jev_model: None,
                    detail: Some(format!("attempts={attempts}")),
                },
            )?;
        }
        tx.commit()?;
        Ok(failed)
    }

    /// Expires every pending `lane`, `subagent` and `scan` job created more than
    /// `ttl_seconds` before `now` (payload cleared, one [`M_JOB_EXPIRED`] log row each), in
    /// one transaction. `group` jobs never expire. Returns how many expired.
    pub fn expire_memory_jobs(
        &mut self,
        now: i64,
        ttl_seconds: i64,
        omp_version: &str,
    ) -> Result<usize, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expired: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "UPDATE memory_jobs SET state = 'expired', payload = NULL, updated_at = ?1
                 WHERE state = 'pending' AND kind IN ('lane','subagent','scan')
                   AND created_at < ?1 - ?2
                 RETURNING id, kind",
            )?;
            stmt.query_map(params![now, ttl_seconds], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        for (id, kind) in &expired {
            insert_log(
                &tx,
                &LogRow {
                    at: now,
                    level: LogLevel::Info,
                    code: M_JOB_EXPIRED,
                    job_id: Some(*id),
                    omp_version: Some(omp_version.to_owned()),
                    jev_model: None,
                    detail: Some(format!("kind={kind}")),
                },
            )?;
        }
        tx.commit()?;
        Ok(expired.len())
    }

    /// Whether subagent output `path` still has the `size` and `mtime` (nanoseconds) its cursor
    /// row holds; the scan then skips the file without reading it.
    pub fn artifact_unchanged(
        &self,
        path: &str,
        size: i64,
        mtime: i64,
    ) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_cursor WHERE path = ?1 AND size = ?2 AND mtime = ?3)",
            params![path, size, mtime],
            |r| r.get(0),
        )?)
    }

    /// Records subagent output `job.path` with `size`, `mtime` (nanoseconds) and `job.sha256`:
    /// when its cursor row already holds that size and hash only the mtime is updated (false);
    /// otherwise the cursor row is written and one `subagent` job is stored, in one
    /// transaction (true). A file is so queued once per content.
    pub fn note_artifact(
        &mut self,
        job: &SubagentJob,
        size: i64,
        mtime: i64,
        now: i64,
    ) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unchanged = tx.execute(
            "UPDATE memory_cursor SET mtime = ?4 WHERE path = ?1 AND size = ?2 AND sha256 = ?3",
            params![job.path, size, job.sha256, mtime],
        )? > 0;
        if unchanged {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO memory_cursor(path, size, mtime, sha256, seen_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET size = ?2, mtime = ?3, sha256 = ?4, seen_at = ?5",
            params![job.path, size, mtime, job.sha256, now],
        )?;
        let inserted = insert_job(
            &tx,
            &format!("{} {}", job.sha256, job.path),
            false,
            &JobPayload::Subagent(job.clone()),
            Some(now),
        )?;
        tx.commit()?;
        self.wake_after(inserted);
        Ok(true)
    }

    /// Whether a subagent output file under `prefix` (a session folder path ending in `/`) was
    /// last modified at or after inbox row `row` was received (less one second, for the
    /// coarse clock file times use): the files a turn's subagents wrote, whichever scan found
    /// them first.
    pub fn artifact_written_since(&self, prefix: &str, row: i64) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_cursor
               WHERE substr(path, 1, length(?1)) = ?1
                 AND mtime >= (SELECT (CAST(strftime('%s', received_at) AS INTEGER) - 1) * 1000000000
                               FROM inbox WHERE id = ?2))",
            params![prefix, row],
            |r| r.get(0),
        )?)
    }

    /// Writes one `memory_log` row.
    pub fn memory_log(&self, row: &LogRow) -> Result<(), StoreError> {
        insert_log(&self.conn, row)?;
        Ok(())
    }

    /// Whether a `memory_log` row with `code` and exactly `detail` exists.
    pub fn memory_logged(&self, code: &str, detail: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_log WHERE code = ?1 AND detail = ?2)",
            params![code, detail],
            |r| r.get(0),
        )?)
    }

    /// Pending memory jobs: `group` jobs, and every other kind.
    pub fn memory_backlog(&self) -> Result<Backlog, StoreError> {
        let (groups, other): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(kind = 'group'), 0), COALESCE(SUM(kind != 'group'), 0)
             FROM memory_jobs WHERE state = 'pending'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Backlog {
            groups: groups as usize,
            other: other as usize,
        })
    }

    /// Whether the lead has been quiet for `seconds` before `now` (unix seconds): no inbox row
    /// is queued or being processed, and none changed in that time. Self consolidation (#59)
    /// runs only then, so the lead restart a changed self block brings costs no waiting owner.
    pub fn lead_quiet(&self, now: i64, seconds: i64) -> Result<bool, StoreError> {
        let (busy, last): (i64, Option<i64>) = self.conn.query_row(
            "SELECT COALESCE(SUM(state IN ('queued', 'processing')), 0),
                    MAX(CAST(strftime('%s', updated_at) AS INTEGER))
             FROM inbox",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(busy == 0 && last.is_none_or(|at| now - at >= seconds))
    }

    /// Deletes memory log rows and settled memory jobs older than 30 days before `now`.
    /// Pending jobs and cursor rows are kept.
    pub fn prune_memory(&mut self, now: i64) -> Result<(), StoreError> {
        let cutoff = now - MEMORY_KEEP_SECONDS;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM memory_log WHERE at < ?1", [cutoff])?;
        tx.execute(
            "DELETE FROM memory_jobs WHERE state != 'pending' AND updated_at < ?1",
            [cutoff],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn journal_mode(&self) -> Result<String, StoreError> {
        Ok(self
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))?)
    }

    /// Stores an inbound message and its conversation in one transaction.
    /// A repeated `(platform, message_id)` is reported as `Duplicate` and changes nothing.
    pub fn admit(
        &mut self,
        platform: &str,
        message_id: &str,
        conversation_key: &str,
        channel_id: &str,
        author_id: &str,
        text: &str,
    ) -> Result<Admitted, StoreError> {
        self.admit_with_files(
            platform,
            message_id,
            conversation_key,
            channel_id,
            author_id,
            text,
            None,
        )
    }

    /// [`Store::admit`] with the declared attachment JSON. A row with attachments waits
    /// (`files_ready = 0`) until [`Store::files_saved`] or [`Store::files_failed`].
    #[allow(clippy::too_many_arguments)]
    pub fn admit_with_files(
        &mut self,
        platform: &str,
        message_id: &str,
        conversation_key: &str,
        channel_id: &str,
        author_id: &str,
        text: &str,
        attachments: Option<&str>,
    ) -> Result<Admitted, StoreError> {
        if conversation_key.is_empty() {
            return Err(StoreError::Invalid("conversation_key must not be empty"));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            &format!(
                "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state, received_at, updated_at, attachments, files_ready)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', {NOW}, {NOW}, ?6, ?7)
                 ON CONFLICT(platform, message_id) DO NOTHING"
            ),
            params![
                platform,
                message_id,
                conversation_key,
                author_id,
                text,
                attachments,
                attachments.is_none()
            ],
        )?;
        if inserted == 0 {
            return Ok(Admitted::Duplicate);
        }
        let id = tx.last_insert_rowid();
        tx.execute(
            &format!(
                "INSERT INTO conversations(conversation_key, platform, channel_id, created_at, last_activity_at)
                 VALUES (?1, ?2, ?3, {NOW}, {NOW})
                 ON CONFLICT(conversation_key) DO UPDATE SET last_activity_at = excluded.last_activity_at"
            ),
            params![conversation_key, platform, channel_id],
        )?;
        tx.commit()?;
        Ok(Admitted::New(id))
    }

    pub fn get(&self, id: i64) -> Result<Option<InboxRow>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLS} FROM inbox WHERE id = ?1"),
                [id],
                map_row,
            )
            .optional()?)
    }

    /// Total number of stored inbox rows.
    pub fn count(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM inbox", [], |r| r.get(0))?)
    }

    /// Oldest queued row across all conversations (one global FIFO).
    pub fn next_queued(&self) -> Result<Option<InboxRow>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLS} FROM inbox WHERE state = 'queued' AND files_ready = 1 ORDER BY id LIMIT 1"
                ),
                [],
                map_row,
            )
            .optional()?)
    }

    /// Queued rows that start their own thread: key `<platform>:th:<own message_id>`.
    pub fn queued_thread_starters(&self) -> Result<Vec<InboxRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM inbox
             WHERE state = 'queued' AND conversation_key = platform || ':th:' || message_id
             ORDER BY id"
        ))?;
        Ok(stmt.query_map([], map_row)?.collect::<Result<_, _>>()?)
    }

    /// The channel a conversation was started in.
    pub fn channel_of(&self, conversation_key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT channel_id FROM conversations WHERE conversation_key = ?1",
                [conversation_key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn mark(&self, id: i64, state: State, error: Option<&str>) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            &format!("UPDATE inbox SET state = ?2, error = ?3, updated_at = {NOW} WHERE id = ?1"),
            params![id, state.as_str(), error],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such inbox row"));
        }
        Ok(())
    }

    /// Persists the reply text and its outbound-file JSON and moves the row to
    /// `reply_pending`, all in one statement.
    pub fn save_reply(
        &self,
        id: i64,
        reply: &str,
        out_files: Option<&str>,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE inbox SET reply = ?2, out_files = ?3, state = 'reply_pending', error = NULL, updated_at = {NOW} WHERE id = ?1"
            ),
            params![id, reply, out_files],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such inbox row"));
        }
        Ok(())
    }

    /// Stores the saved-file JSON and releases a still-queued row to the FIFO.
    /// Returns false (and changes nothing) when the row is no longer `queued`.
    pub fn files_saved(&self, id: i64, attachments_json: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            &format!(
                "UPDATE inbox SET attachments = ?2, files_ready = 1, updated_at = {NOW}
                 WHERE id = ?1 AND state = 'queued'"
            ),
            params![id, attachments_json],
        )? != 0)
    }

    /// Fails a still-queued row whose files could not be saved, with its notice to post.
    /// Returns false (and changes nothing) when the row is no longer `queued`.
    pub fn files_failed(&self, id: i64, code: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            &format!(
                "UPDATE inbox SET state = 'failed', error = ?2, notice_sent = 0, updated_at = {NOW}
                 WHERE id = ?1 AND state = 'queued'"
            ),
            params![id, code],
        )? != 0)
    }

    /// Records that the reply text was fully delivered.
    pub fn mark_text_sent(&self, id: i64) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            &format!("UPDATE inbox SET text_sent = 1, updated_at = {NOW} WHERE id = ?1"),
            [id],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such inbox row"));
        }
        Ok(())
    }

    /// Marks outbound file `index` sent and clears its in-flight marker.
    pub fn mark_file_sent(&self, id: i64, index: usize) -> Result<(), StoreError> {
        update_out_file(
            &self.conn,
            id,
            index,
            "$e || '.sent', json('true'), $e || '.attempting', json('false')",
        )
    }

    /// Marks outbound file `index` skipped and clears its in-flight marker.
    pub fn mark_file_skipped(&self, id: i64, index: usize) -> Result<(), StoreError> {
        update_out_file(
            &self.conn,
            id,
            index,
            "$e || '.skipped', json('true'), $e || '.attempting', json('false')",
        )
    }

    /// Marks outbound file `index` in flight and counts the attempt, in one committed
    /// statement, before any byte of the upload leaves.
    pub fn mark_file_attempting(&self, id: i64, index: usize) -> Result<(), StoreError> {
        update_out_file(
            &self.conn,
            id,
            index,
            "$e || '.attempting', json('true'),
             $e || '.attempts', coalesce(json_extract(out_files, $e || '.attempts'), 0) + 1",
        )
    }

    /// Clears the in-flight marker of outbound file `index` after its upload call returned.
    pub fn clear_file_attempting(&self, id: i64, index: usize) -> Result<(), StoreError> {
        update_out_file(&self.conn, id, index, "$e || '.attempting', json('false')")
    }

    /// Marks every row of an interrupted group `needs_check` with `code` and, in the same
    /// transaction, holds the queued owner rows of their conversations, so the hold survives
    /// a restart. The group's `partial` memory job is stored in the same transaction.
    pub fn interrupt_group(
        &mut self,
        ids: &[i64],
        code: &str,
        trace: &TurnTrace,
    ) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for &id in ids {
            let changed = tx.execute(
                &format!("UPDATE inbox SET state = 'needs_check', error = ?2, updated_at = {NOW} WHERE id = ?1"),
                params![id, code],
            )?;
            if changed == 0 {
                return Err(StoreError::Invalid("no such inbox row"));
            }
        }
        for &id in ids {
            tx.execute(
                &hold_queued_sql("SELECT conversation_key FROM inbox WHERE id = ?1"),
                [id],
            )?;
        }
        let inserted = enqueue_group(&tx, ids, true, Some(trace))?;
        tx.commit()?;
        self.wake_after(inserted);
        Ok(())
    }

    /// Claims a queued row for one lead turn. Returns false (and changes nothing) when the
    /// row is no longer `queued`, so one row is run by one path only.
    pub fn claim(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            &format!(
                "UPDATE inbox SET state = 'processing', updated_at = {NOW}
                 WHERE id = ?1 AND state = 'queued'"
            ),
            [id],
        )? != 0)
    }

    /// Claims a queued row as the steer of `main_id`'s running turn. Refused (false, nothing
    /// changed) unless the row is still `queued`, newer than `main_id`, and no older row of
    /// its conversation is still `queued`.
    pub fn steer_into(&self, id: i64, main_id: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            &format!(
                "UPDATE inbox SET state = 'processing', steered_into = ?2, updated_at = {NOW}
                 WHERE id = ?1 AND state = 'queued' AND ?1 > ?2
                   AND NOT EXISTS (SELECT 1 FROM inbox o
                                   WHERE o.conversation_key = inbox.conversation_key
                                     AND o.state = 'queued' AND o.id < ?1)"
            ),
            params![id, main_id],
        )? != 0)
    }

    /// Stores a turn group's reply on its main row (`reply_pending`, as
    /// [`Store::save_reply`]) and marks the steered row, if any, `done`, in one transaction
    /// that also stores the group's memory job.
    pub fn finish_group(
        &mut self,
        main_id: i64,
        reply: &str,
        out_files: Option<&str>,
        steered: Option<i64>,
        trace: &TurnTrace,
    ) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            &format!(
                "UPDATE inbox SET reply = ?2, out_files = ?3, state = 'reply_pending', error = NULL, updated_at = {NOW} WHERE id = ?1"
            ),
            params![main_id, reply, out_files],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such inbox row"));
        }
        if let Some(id) = steered {
            let changed = tx.execute(
                &format!(
                    "UPDATE inbox SET state = 'done', error = NULL, updated_at = {NOW} WHERE id = ?1"
                ),
                [id],
            )?;
            if changed == 0 {
                return Err(StoreError::Invalid("no such inbox row"));
            }
        }
        let ids: Vec<i64> = std::iter::once(main_id).chain(steered).collect();
        let inserted = enqueue_group(&tx, &ids, false, Some(trace))?;
        tx.commit()?;
        self.wake_after(inserted);
        Ok(())
    }

    /// Stores the memory store's answer to an owner memory command on `id`
    /// (`reply_pending`, as [`Store::save_reply`]) and marks the row `memory_answered`, so
    /// it is never counted as an onboarding turn.
    pub fn finish_memory(&mut self, id: i64, reply: &str) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            &format!(
                "UPDATE inbox SET reply = ?2, out_files = NULL, state = 'reply_pending', error = NULL, memory_answered = 1, updated_at = {NOW} WHERE id = ?1"
            ),
            params![id, reply],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such inbox row"));
        }
        tx.commit()?;
        Ok(())
    }

    /// Marks every row of a turn group `failed` with `code`, in one transaction that also
    /// stores the group's `partial` memory job.
    pub fn fail_group(
        &mut self,
        ids: &[i64],
        code: &str,
        trace: &TurnTrace,
    ) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for &id in ids {
            let changed = tx.execute(
                &format!(
                    "UPDATE inbox SET state = 'failed', error = ?2, updated_at = {NOW} WHERE id = ?1"
                ),
                params![id, code],
            )?;
            if changed == 0 {
                return Err(StoreError::Invalid("no such inbox row"));
            }
        }
        let inserted = enqueue_group(&tx, ids, true, Some(trace))?;
        tx.commit()?;
        self.wake_after(inserted);
        Ok(())
    }

    pub fn mark_done(&self, id: i64) -> Result<(), StoreError> {
        self.mark(id, State::Done, None)
    }

    pub fn mark_notice_sent(&self, id: i64) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            &format!("UPDATE inbox SET notice_sent = 1, updated_at = {NOW} WHERE id = ?1"),
            [id],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such inbox row"));
        }
        Ok(())
    }

    /// Startup recovery, in one transaction: `processing` becomes `needs_check` (never
    /// re-queued) and the queued rows of its conversation are held; then queued rows still
    /// waiting for their files fail with [`E_FILE_INCOMPLETE`]; outbound files whose upload
    /// was cut are skipped (row ids in `interrupted_uploads`); `reply_pending` rows are
    /// returned for re-sending. Each cut group (rows grouped by the turn they ran in) gets
    /// its `partial` memory job without a trace, so the owner's words are still observed.
    pub fn recover_on_start(&mut self) -> Result<Recovered, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cut: Vec<(i64, Option<i64>)> = {
            let mut stmt = tx.prepare(
                "SELECT id, steered_into FROM inbox WHERE state = 'processing' ORDER BY id",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        let ids: Vec<i64> = cut.iter().map(|(id, _)| *id).collect();
        // Ordered by id, so each group lists its main row (the older one) first.
        let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for (id, steered_into) in cut {
            groups
                .entry(steered_into.unwrap_or(id))
                .or_default()
                .push(id);
        }
        let mut inserted = false;
        for group in groups.values() {
            inserted |= enqueue_group(&tx, group, true, None)?;
        }
        tx.execute(
            &hold_queued_sql("SELECT conversation_key FROM inbox WHERE state = 'processing'"),
            [],
        )?;
        tx.execute(
            &format!(
                "UPDATE inbox SET state = 'needs_check', error = 'interrupted', updated_at = {NOW}
                 WHERE state = 'processing'"
            ),
            [],
        )?;
        // Lanes are never restarted: a running one is marked for one restart notice.
        tx.execute(
            &format!(
                "UPDATE lanes SET state = 'needs_check', error = '{E_LANE_INTERRUPTED}', notice_sent = 0, updated_at = {NOW}
                 WHERE state = 'running'"
            ),
            [],
        )?;
        let mut needs_check = Vec::with_capacity(ids.len());
        for id in ids {
            needs_check.push(tx.query_row(
                &format!("SELECT {COLS} FROM inbox WHERE id = ?1"),
                [id],
                map_row,
            )?);
        }
        // After the hold above, so a waiting row of an interrupted conversation stays held.
        tx.execute(
            &format!(
                "UPDATE inbox SET state = 'failed', error = '{E_FILE_INCOMPLETE}', notice_sent = 0, updated_at = {NOW}
                 WHERE state = 'queued' AND files_ready = 0"
            ),
            [],
        )?;
        let mut interrupted_uploads = Vec::new();
        {
            let pending: Vec<(i64, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, out_files FROM inbox
                     WHERE state = 'reply_pending' AND out_files IS NOT NULL ORDER BY id",
                )?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            for (id, out_files) in pending {
                if let Some(updated) = skip_interrupted(&out_files) {
                    tx.execute(
                        &format!(
                            "UPDATE inbox SET out_files = ?2, updated_at = {NOW} WHERE id = ?1"
                        ),
                        params![id, updated],
                    )?;
                    interrupted_uploads.push(id);
                }
            }
        }
        let reply_pending = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {COLS} FROM inbox WHERE state = 'reply_pending' ORDER BY id"
            ))?;
            stmt.query_map([], map_row)?.collect::<Result<_, _>>()?
        };
        tx.commit()?;
        self.wake_after(inserted);
        Ok(Recovered {
            needs_check,
            reply_pending,
            interrupted_uploads,
        })
    }

    /// `failed` rows with a file error whose notice has not been posted yet.
    pub fn failed_notices(&self) -> Result<Vec<InboxRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM inbox
             WHERE state = 'failed' AND notice_sent = 0 AND error LIKE 'E\\_FILE\\_%' ESCAPE '\\'
             ORDER BY id"
        ))?;
        Ok(stmt.query_map([], map_row)?.collect::<Result<_, _>>()?)
    }

    /// `needs_check` rows whose notice has not been posted yet.
    pub fn needs_check_notices(&self) -> Result<Vec<InboxRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM inbox WHERE state = 'needs_check' AND notice_sent = 0 ORDER BY id"
        ))?;
        Ok(stmt.query_map([], map_row)?.collect::<Result<_, _>>()?)
    }

    /// The omp session id stored under `name` (for example the lead's).
    pub fn session_id(&self, name: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT omp_session_id FROM sessions WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Stores (or replaces) the omp session id under `name`.
    pub fn set_session_id(&self, name: &str, session_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO sessions(name, omp_session_id, updated_at) VALUES (?1, ?2, {NOW})
                 ON CONFLICT(name) DO UPDATE SET omp_session_id = excluded.omp_session_id,
                                                 updated_at = excluded.updated_at"
            ),
            params![name, session_id],
        )?;
        Ok(())
    }

    /// Stores a new running lane for a `lane_start` tool call, in one transaction. A
    /// repeated `tool_call_id` returns the lane it already started; at `max_running`
    /// running lanes nothing is stored.
    pub fn start_lane(
        &mut self,
        tool_call_id: &str,
        conversation_key: &str,
        task: &str,
        done: Option<&str>,
        repo: Option<&str>,
        max_running: usize,
    ) -> Result<LaneStart, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM lanes WHERE tool_call_id = ?1",
                [tool_call_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(LaneStart::Existing(id));
        }
        if running_lanes(&tx)? >= max_running {
            return Ok(LaneStart::Limit);
        }
        tx.execute(
            &format!(
                "INSERT INTO lanes(tool_call_id, conversation_key, task, done, repo, state, prompts, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'running', 1, {NOW}, {NOW})"
            ),
            params![tool_call_id, conversation_key, task, done, repo],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(LaneStart::New(id))
    }

    /// Moves a lane that is not running back to `running` for one more prompt, in one
    /// transaction, and returns that prompt's sequence number.
    pub fn message_lane(&mut self, id: i64, max_running: usize) -> Result<LaneMessage, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT state, closed_at FROM lanes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => return Ok(LaneMessage::Unknown),
            Some((_, Some(_))) => return Ok(LaneMessage::Closed),
            Some((state, None)) if state == "running" => return Ok(LaneMessage::Busy),
            Some(_) => {}
        }
        if running_lanes(&tx)? >= max_running {
            return Ok(LaneMessage::Limit);
        }
        let seq: i64 = tx.query_row(
            &format!(
                "UPDATE lanes SET state = 'running', error = NULL, prompts = prompts + 1, updated_at = {NOW}
                 WHERE id = ?1 RETURNING prompts"
            ),
            [id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(LaneMessage::Seq(seq))
    }

    /// Marks a lane that is not running as closed, in one transaction; a second call keeps the
    /// first time and reason. The first close also stores the `scan` memory job of the lane's
    /// session folder.
    pub fn close_lane(&mut self, id: i64, reason: &str) -> Result<LaneClose, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, Option<String>, bool)> = tx
            .query_row(
                "SELECT state, repo, closed_at IS NOT NULL FROM lanes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, repo, closed_before)) = row else {
            return Ok(LaneClose::Unknown);
        };
        if state == "running" {
            return Ok(LaneClose::Busy);
        }
        tx.execute(
            &format!(
                "UPDATE lanes SET closed_at = COALESCE(closed_at, {NOW}), close_reason = COALESCE(close_reason, ?2), updated_at = {NOW}
                 WHERE id = ?1"
            ),
            params![id, reason],
        )?;
        let inserted = !closed_before
            && insert_job(
                &tx,
                &format!("lane:{id}"),
                false,
                &JobPayload::Scan(ScanJob { lane: id }),
                None,
            )?;
        tx.commit()?;
        self.wake_after(inserted);
        Ok(LaneClose::Closed { repo })
    }

    pub fn lane(&self, id: i64) -> Result<Option<LaneRow>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {LANE_COLS} FROM lanes WHERE id = ?1"),
                [id],
                map_lane,
            )
            .optional()?)
    }

    /// Every lane, oldest first.
    pub fn lanes(&self) -> Result<Vec<LaneRow>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {LANE_COLS} FROM lanes ORDER BY id"))?;
        Ok(stmt.query_map([], map_lane)?.collect::<Result<_, _>>()?)
    }

    pub fn set_lane_session_id(&self, id: i64, session_id: &str) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            &format!("UPDATE lanes SET omp_session_id = ?2, updated_at = {NOW} WHERE id = ?1"),
            params![id, session_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such lane"));
        }
        Ok(())
    }

    /// Stores lane prompt `seq`'s result as a queued inbox row of the lane's conversation
    /// and sets the lane's state, in one transaction. A result without `error` also stores
    /// its `lane` memory job there; a failed, stopped or unmet one gets none. A second call
    /// for the same `seq` changes nothing and returns false.
    pub fn finish_lane(
        &mut self,
        id: i64,
        seq: i64,
        text: &str,
        state: LaneState,
        error: Option<&str>,
    ) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key: String = tx
            .query_row(
                "SELECT conversation_key FROM lanes WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::Invalid("no such lane"))?;
        let inserted = tx.execute(
            &format!(
                "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state, received_at, updated_at, files_ready, origin)
                 VALUES (?1, ?2, ?3, 'lane', ?4, 'queued', {NOW}, {NOW}, 1, 'lane')
                 ON CONFLICT(platform, message_id) DO NOTHING"
            ),
            params![LANE_PLATFORM, format!("{id}:{seq}"), key, text],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        let row = tx.last_insert_rowid();
        tx.execute(
            &format!("UPDATE lanes SET state = ?2, error = ?3, updated_at = {NOW} WHERE id = ?1"),
            params![id, state.as_str(), error],
        )?;
        let queued = error.is_none()
            && insert_job(
                &tx,
                &format!("{id}:{seq}"),
                false,
                &JobPayload::Lane(LaneJob { row, lane: id, seq }),
                None,
            )?;
        tx.commit()?;
        self.wake_after(queued);
        Ok(true)
    }

    /// Queues one stalled-lane notice for job `(id, seq)` as a lane inbox row
    /// (`message_id = "<id>:<seq>:stalled:<episode>"`) without touching the lane's state.
    /// `Ok(false)` when that row already exists or the lane is no longer running.
    pub fn stall_notice(
        &mut self,
        id: i64,
        seq: i64,
        episode: u32,
        text: &str,
    ) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (key, state): (String, String) = tx
            .query_row(
                "SELECT conversation_key, state FROM lanes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::Invalid("no such lane"))?;
        if state != LaneState::Running.as_str() {
            return Ok(false);
        }
        let inserted = tx.execute(
            &format!(
                "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state, received_at, updated_at, files_ready, origin)
                 VALUES (?1, ?2, ?3, 'lane', ?4, 'queued', {NOW}, {NOW}, 1, 'lane')
                 ON CONFLICT(platform, message_id) DO NOTHING"
            ),
            params![LANE_PLATFORM, format!("{id}:{seq}:stalled:{episode}"), key, text],
        )?;
        tx.commit()?;
        Ok(inserted == 1)
    }

    /// Lanes a restart interrupted whose notice has not been posted yet.
    pub fn lane_notices(&self) -> Result<Vec<LaneRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {LANE_COLS} FROM lanes
             WHERE state = 'needs_check' AND error = '{E_LANE_INTERRUPTED}' AND notice_sent = 0
             ORDER BY id"
        ))?;
        Ok(stmt.query_map([], map_lane)?.collect::<Result<_, _>>()?)
    }

    pub fn mark_lane_notice_sent(&self, id: i64) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            &format!("UPDATE lanes SET notice_sent = 1, updated_at = {NOW} WHERE id = ?1"),
            [id],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such lane"));
        }
        Ok(())
    }

    pub fn remember_thread(&self, platform: &str, thread_id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO known_threads(platform, thread_id) VALUES (?1, ?2)",
            params![platform, thread_id],
        )?;
        Ok(())
    }

    pub fn is_known_thread(&self, platform: &str, thread_id: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM known_threads WHERE platform = ?1 AND thread_id = ?2",
                params![platform, thread_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// The onboarding row, `None` before the first request.
    pub fn onboarding(&self) -> Result<Option<OnboardingRow>, StoreError> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT state, after_row FROM onboarding WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(state, after_row)| {
            OnboardingState::parse(&state)
                .map(|state| OnboardingRow { state, after_row })
                .ok_or(StoreError::Invalid("unknown onboarding state"))
        })
        .transpose()
    }

    /// Starts (or restarts) the onboarding after the newest inbox row; returns that row id
    /// (0 for an empty inbox).
    pub fn start_onboarding(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            &format!(
                "INSERT INTO onboarding(id, state, after_row, started_at, ended_at)
                 VALUES (1, 'active', COALESCE((SELECT MAX(id) FROM inbox), 0), {NOW}, NULL)
                 ON CONFLICT(id) DO UPDATE SET state = 'active', after_row = excluded.after_row,
                   started_at = excluded.started_at, ended_at = NULL
                 RETURNING after_row"
            ),
            [],
            |r| r.get(0),
        )?)
    }

    /// Ends a running onboarding as `end` (`Done` or `Skipped`): false when none is running.
    pub fn finish_onboarding(&self, end: OnboardingState) -> Result<bool, StoreError> {
        if end == OnboardingState::Active {
            return Err(StoreError::Invalid("onboarding ends as done or skipped"));
        }
        Ok(self.conn.execute(
            &format!(
                "UPDATE onboarding SET state = ?1, ended_at = {NOW} WHERE id = 1 AND state = 'active'"
            ),
            [end.as_str()],
        )? == 1)
    }

    /// The owner turn number of `turn_row` in a running onboarding: owner rows after its
    /// start up to and including `turn_row` that were neither steered into another row,
    /// answered by the memory store, nor left queued or failed. `None` unless an
    /// onboarding is running.
    pub fn onboarding_turn(&self, turn_row: i64) -> Result<Option<i64>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM inbox
                         WHERE origin = 'owner' AND steered_into IS NULL
                           AND memory_answered = 0
                           AND state NOT IN ('queued', 'failed')
                           AND id > onboarding.after_row AND id <= ?1)
                 FROM onboarding WHERE id = 1 AND state = 'active'",
                [turn_row],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_tmp() -> (TempDir, std::path::PathBuf, Store) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let store = Store::open(&path).unwrap();
        (dir, path, store)
    }

    fn admit(s: &mut Store, mid: &str, key: &str) -> Admitted {
        s.admit("test", mid, key, "chan1", "111", "hello").unwrap()
    }

    fn new_id(a: Admitted) -> i64 {
        match a {
            Admitted::New(id) => id,
            Admitted::Duplicate => panic!("expected New"),
        }
    }

    #[test]
    fn dedup_same_message_id_one_row() {
        let (_d, path, mut s) = open_tmp();
        assert!(matches!(
            admit(&mut s, "m1", "test:th:m1"),
            Admitted::New(_)
        ));
        assert_eq!(admit(&mut s, "m1", "test:th:m1"), Admitted::Duplicate);
        assert_eq!(s.count().unwrap(), 1);
        drop(s);
        let mut s = Store::open(&path).unwrap();
        assert_eq!(admit(&mut s, "m1", "test:th:m1"), Admitted::Duplicate);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn restart_marks_processing_needs_check() {
        let (_d, path, mut s) = open_tmp();
        let id = new_id(admit(&mut s, "m1", "k1"));
        s.mark(id, State::Processing, None).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        let rec = s.recover_on_start().unwrap();
        assert_eq!(rec.needs_check.len(), 1);
        assert_eq!(rec.needs_check[0].id, id);
        assert_eq!(rec.needs_check[0].state, State::NeedsCheck);
        assert!(rec.reply_pending.is_empty());
        assert!(s.next_queued().unwrap().is_none());
        assert_eq!(s.get(id).unwrap().unwrap().state, State::NeedsCheck);
        assert_eq!(s.needs_check_notices().unwrap().len(), 1);
    }

    #[test]
    fn restart_keeps_queued_order() {
        let (_d, path, mut s) = open_tmp();
        let a = new_id(admit(&mut s, "m1", "k1"));
        let b = new_id(admit(&mut s, "m2", "k2"));
        let c = new_id(admit(&mut s, "m3", "k1"));
        drop(s);
        let mut s = Store::open(&path).unwrap();
        assert!(s.recover_on_start().unwrap().needs_check.is_empty());
        // One global FIFO across conversations.
        for expected in [a, b, c] {
            let row = s.next_queued().unwrap().unwrap();
            assert_eq!(row.id, expected);
            s.mark_done(row.id).unwrap();
        }
        assert!(s.next_queued().unwrap().is_none());
    }

    #[test]
    fn admit_is_atomic() {
        let (_d, _p, mut s) = open_tmp();
        s.conn
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON conversations
                 BEGIN SELECT RAISE(ABORT, 'forced'); END;",
            )
            .unwrap();
        assert!(
            s.admit("test", "m1", "k1", "chan1", "111", "hello")
                .is_err()
        );
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn empty_conversation_key_is_rejected_and_leaves_table_unchanged() {
        let (_d, _p, mut s) = open_tmp();
        let err = s
            .admit("test", "m1", "", "chan1", "111", "hello")
            .unwrap_err();
        assert_eq!(err.code(), E_STORE_INVALID);
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn journal_mode_is_wal() {
        let (_d, _p, s) = open_tmp();
        assert_eq!(s.journal_mode().unwrap(), "wal");
    }

    #[test]
    fn interruption_holds_queued_rows_of_its_conversation_only() {
        let (_d, path, mut s) = open_tmp();
        let a = new_id(admit(&mut s, "m1", "k1"));
        let b = new_id(admit(&mut s, "m2", "k1"));
        let c = new_id(admit(&mut s, "m3", "k2"));
        let d = new_id(admit(&mut s, "m4", "k2"));
        let e = new_id(admit(&mut s, "m5", "k3"));
        s.mark(a, State::Processing, None).unwrap();
        s.interrupt_group(&[a], "E_TEST", &TurnTrace::default())
            .unwrap();
        s.mark(c, State::Processing, None).unwrap();
        drop(s);

        let mut s = Store::open(&path).unwrap();
        s.recover_on_start().unwrap();
        for (id, error) in [
            (a, "E_TEST"),
            (b, E_DISPATCH_HELD),
            (c, "interrupted"),
            (d, E_DISPATCH_HELD),
        ] {
            let row = s.get(id).unwrap().unwrap();
            assert_eq!(row.state, State::NeedsCheck);
            assert_eq!(row.error.as_deref(), Some(error));
            assert!(!row.notice_sent);
        }
        assert_eq!(s.next_queued().unwrap().unwrap().id, e);
        assert_eq!(s.needs_check_notices().unwrap().len(), 4);
    }

    #[test]
    fn reply_pending_survives_reopen() {
        let (_d, path, mut s) = open_tmp();
        let id = new_id(admit(&mut s, "m1", "k1"));
        s.mark(id, State::Processing, None).unwrap();
        s.save_reply(id, "the answer", None).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        let rec = s.recover_on_start().unwrap();
        assert!(rec.needs_check.is_empty());
        assert_eq!(rec.reply_pending.len(), 1);
        assert_eq!(rec.reply_pending[0].reply.as_deref(), Some("the answer"));
        assert_eq!(s.get(id).unwrap().unwrap().state, State::ReplyPending);
        assert!(s.next_queued().unwrap().is_none());
        s.mark_done(id).unwrap();
        assert_eq!(s.get(id).unwrap().unwrap().state, State::Done);
    }

    #[test]
    fn notice_sent_persists() {
        let (_d, path, mut s) = open_tmp();
        let id = new_id(admit(&mut s, "m1", "k1"));
        s.mark(id, State::Processing, None).unwrap();
        s.recover_on_start().unwrap();
        assert_eq!(s.needs_check_notices().unwrap().len(), 1);
        s.mark_notice_sent(id).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        s.recover_on_start().unwrap();
        assert!(s.get(id).unwrap().unwrap().notice_sent);
        assert!(s.needs_check_notices().unwrap().is_empty());
    }

    #[test]
    fn second_open_fails_with_already_running() {
        let (_d, path, _s) = open_tmp();
        let err = Store::open(&path).err().expect("second open must fail");
        assert!(matches!(err, StoreError::AlreadyRunning));
        assert_eq!(err.code(), E_ALREADY_RUNNING);
    }

    #[test]
    fn closed_resolves_once_the_store_released_the_database() {
        let (_d, path, mut s) = open_tmp();
        let closed = s.closed();
        let reopen = std::thread::spawn(move || {
            let released = closed.wait(Duration::from_secs(30));
            (released, Store::open(&path).map(drop))
        });
        drop(s);
        let (released, reopened) = reopen.join().unwrap();
        assert!(released);
        assert!(reopened.is_ok(), "{:?}", reopened.err());
    }

    #[test]
    fn closed_times_out_while_the_store_is_open() {
        let (_d, _path, mut s) = open_tmp();
        assert!(!s.closed().wait(Duration::from_millis(10)));
    }

    #[test]
    fn known_threads_are_remembered() {
        let (_d, path, s) = open_tmp();
        assert!(!s.is_known_thread("test", "t1").unwrap());
        s.remember_thread("test", "t1").unwrap();
        s.remember_thread("test", "t1").unwrap();
        assert!(s.is_known_thread("test", "t1").unwrap());
        assert!(!s.is_known_thread("other", "t1").unwrap());
        drop(s);
        let s = Store::open(&path).unwrap();
        assert!(s.is_known_thread("test", "t1").unwrap());
    }

    /// The slice-2 inbox table, written by hand so the migration is tested against it.
    const SLICE2_SCHEMA: &str = "
        CREATE TABLE inbox(
            id INTEGER PRIMARY KEY,
            platform TEXT NOT NULL,
            message_id TEXT NOT NULL,
            conversation_key TEXT NOT NULL,
            author_id TEXT NOT NULL,
            text TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('queued','processing','reply_pending','done','failed','needs_check')),
            reply TEXT,
            notice_sent INTEGER NOT NULL DEFAULT 0,
            received_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            error TEXT,
            UNIQUE(platform, message_id)
        );
        CREATE TABLE conversations(
            conversation_key TEXT PRIMARY KEY,
            platform TEXT,
            channel_id TEXT NOT NULL,
            omp_session_id TEXT,
            created_at TEXT,
            last_activity_at TEXT
        );
        CREATE TABLE known_threads(
            platform TEXT,
            thread_id TEXT,
            PRIMARY KEY(platform, thread_id)
        );
    ";

    const ALL_STATES: [&str; 6] = [
        "queued",
        "processing",
        "reply_pending",
        "done",
        "failed",
        "needs_check",
    ];

    /// A slice-2 database file with one row in each state.
    fn slice2_db() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SLICE2_SCHEMA).unwrap();
        for (i, state) in ALL_STATES.iter().enumerate() {
            conn.execute(
                "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state, reply, received_at, updated_at)
                 VALUES ('test', ?1, ?2, '111', 'hello', ?3, 'r', 't', 't')",
                params![format!("m{i}"), format!("k{i}"), state],
            )
            .unwrap();
        }
        drop(conn);
        (dir, path)
    }

    fn columns(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('inbox')")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    const NEW_COLUMNS: [&str; 4] = ["attachments", "files_ready", "out_files", "text_sent"];

    fn admit_files(s: &mut Store, mid: &str, key: &str) -> i64 {
        new_id(
            s.admit_with_files(
                "test",
                mid,
                key,
                "chan1",
                "111",
                "",
                Some(r#"[{"name":"a.txt","size":3,"mime":null}]"#),
            )
            .unwrap(),
        )
    }

    /// A `reply_pending` row with `n` outbound files that have never been attempted.
    fn reply_with_files(s: &mut Store, mid: &str, n: usize) -> i64 {
        let id = new_id(admit(s, mid, "k1"));
        s.mark(id, State::Processing, None).unwrap();
        let files: Vec<Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "path": format!("/work/out{i}.txt"),
                    "name": format!("out{i}.txt"),
                    "sent": false,
                    "skipped": false,
                    "attempting": false,
                    "attempts": 0
                })
            })
            .collect();
        s.save_reply(id, "the answer", Some(&Value::from(files).to_string()))
            .unwrap();
        id
    }

    fn out_file(s: &Store, id: i64, index: usize) -> Value {
        let json = s.get(id).unwrap().unwrap().out_files.unwrap();
        serde_json::from_str::<Value>(&json).unwrap()[index].clone()
    }

    #[test]
    fn slice2_database_gains_columns_and_keeps_rows() {
        let (_d, path) = slice2_db();
        let s = Store::open(&path).unwrap();
        let cols = columns(&s.conn);
        for c in NEW_COLUMNS {
            assert!(cols.iter().any(|x| x == c), "missing column {c}");
        }
        assert_eq!(s.count().unwrap(), ALL_STATES.len() as i64);
        for (i, state) in ALL_STATES.iter().enumerate() {
            let row = s.get(i as i64 + 1).unwrap().unwrap();
            assert_eq!(row.state.as_str(), *state);
            assert_eq!(row.attachments, None);
            assert!(row.files_ready);
            assert_eq!(row.out_files, None);
            assert!(!row.text_sent);
        }
        // The old queued row is still next in the FIFO.
        assert_eq!(s.next_queued().unwrap().unwrap().id, 1);
    }

    #[test]
    fn reopen_is_idempotent() {
        let (_d, path) = slice2_db();
        let s = Store::open(&path).unwrap();
        let first = columns(&s.conn);
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(columns(&s.conn), first);
        drop(s);
        let (_d2, _fresh, s) = open_tmp();
        // A new database ends with the same table as a migrated one.
        assert_eq!(columns(&s.conn), first);
    }

    #[test]
    fn migration_respects_exclusive_lock() {
        let (_d, path) = slice2_db();
        let holder = Store::open(&path).unwrap();
        let err = Store::open(&path).err().expect("second open must fail");
        assert_eq!(err.code(), E_ALREADY_RUNNING);
        drop(holder);

        // A slice-2 file held by another connection is not migrated behind its back.
        let (_d2, path2) = slice2_db();
        let other = Connection::open(&path2).unwrap();
        other
            .pragma_update(None, "locking_mode", "EXCLUSIVE")
            .unwrap();
        other.execute_batch("BEGIN IMMEDIATE; COMMIT;").unwrap();
        let err = Store::open(&path2)
            .err()
            .expect("open must fail while held");
        assert_eq!(err.code(), E_ALREADY_RUNNING);
        let cols = columns(&other);
        for c in NEW_COLUMNS {
            assert!(
                !cols.iter().any(|x| x == c),
                "column {c} added under a held lock"
            );
        }
    }

    #[test]
    fn admit_with_files_waits_for_files() {
        let (_d, _p, mut s) = open_tmp();
        let id = admit_files(&mut s, "m1", "k1");
        let row = s.get(id).unwrap().unwrap();
        assert!(!row.files_ready);
        assert!(row.attachments.is_some());
        let plain = new_id(admit(&mut s, "m2", "k2"));
        let row = s.get(plain).unwrap().unwrap();
        assert!(row.files_ready);
        assert_eq!(row.attachments, None);
    }

    #[test]
    fn row_waiting_for_files_is_not_next() {
        let (_d, _p, mut s) = open_tmp();
        let waiting = admit_files(&mut s, "m1", "k1");
        let plain = new_id(admit(&mut s, "m2", "k2"));
        assert_eq!(s.next_queued().unwrap().unwrap().id, plain);
        s.mark_done(plain).unwrap();
        assert!(s.next_queued().unwrap().is_none());
        assert_eq!(s.get(waiting).unwrap().unwrap().state, State::Queued);
    }

    #[test]
    fn files_saved_makes_row_next() {
        let (_d, _p, mut s) = open_tmp();
        let id = admit_files(&mut s, "m1", "k1");
        let saved = r#"[{"path":"/work/1-a.txt","name":"1-a.txt","mime":null,"bytes":3}]"#;
        assert!(s.files_saved(id, saved).unwrap());
        let row = s.next_queued().unwrap().unwrap();
        assert_eq!(row.id, id);
        assert!(row.files_ready);
        assert_eq!(row.attachments.as_deref(), Some(saved));
        assert_eq!(row.state, State::Queued);
    }

    #[test]
    fn files_failed_is_failed_with_code() {
        let (_d, _p, mut s) = open_tmp();
        let id = admit_files(&mut s, "m1", "k1");
        assert!(s.files_failed(id, crate::files::E_FILE_FETCH).unwrap());
        let row = s.get(id).unwrap().unwrap();
        assert_eq!(row.state, State::Failed);
        assert_eq!(row.error.as_deref(), Some(crate::files::E_FILE_FETCH));
        assert!(!row.notice_sent);
        assert!(s.next_queued().unwrap().is_none());
        let notices = s.failed_notices().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].id, id);
        s.mark_notice_sent(id).unwrap();
        assert!(s.failed_notices().unwrap().is_empty());
        // A second call changes nothing: the row is no longer queued.
        assert!(!s.files_failed(id, crate::files::E_FILE_WRITE).unwrap());
    }

    #[test]
    fn failed_notices_lists_only_file_errors() {
        let (_d, _p, mut s) = open_tmp();
        let other = new_id(admit(&mut s, "m1", "k1"));
        s.mark(other, State::Failed, Some("E_OMP_PROMPT")).unwrap();
        // `_` is a LIKE wildcard: a look-alike code must not match.
        let lookalike = new_id(admit(&mut s, "m2", "k2"));
        s.mark(lookalike, State::Failed, Some("EXFILEX")).unwrap();
        assert!(s.failed_notices().unwrap().is_empty());
    }

    #[test]
    fn files_failed_does_not_override_hold() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_id(admit(&mut s, "m1", "k1"));
        let b = admit_files(&mut s, "m2", "k1");
        s.mark(a, State::Processing, None).unwrap();
        s.interrupt_group(&[a], "E_TEST", &TurnTrace::default())
            .unwrap();
        assert!(!s.files_failed(b, crate::files::E_FILE_FETCH).unwrap());
        let row = s.get(b).unwrap().unwrap();
        assert_eq!(row.state, State::NeedsCheck);
        assert_eq!(row.error.as_deref(), Some(E_DISPATCH_HELD));
    }

    #[test]
    fn files_saved_on_held_row_changes_nothing() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_id(admit(&mut s, "m1", "k1"));
        let b = admit_files(&mut s, "m2", "k1");
        s.mark(a, State::Processing, None).unwrap();
        s.interrupt_group(&[a], "E_TEST", &TurnTrace::default())
            .unwrap();
        let before = s.get(b).unwrap().unwrap();
        assert!(!s.files_saved(b, "[]").unwrap());
        assert_eq!(s.get(b).unwrap().unwrap(), before);
    }

    #[test]
    fn restart_fails_rows_waiting_for_files() {
        let (_d, path, mut s) = open_tmp();
        let waiting = admit_files(&mut s, "m1", "k1");
        let plain = new_id(admit(&mut s, "m2", "k2"));
        // A waiting row of an interrupted conversation stays held instead.
        let a = new_id(admit(&mut s, "m3", "k3"));
        let held = admit_files(&mut s, "m4", "k3");
        s.mark(a, State::Processing, None).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        s.recover_on_start().unwrap();
        let row = s.get(waiting).unwrap().unwrap();
        assert_eq!(row.state, State::Failed);
        assert_eq!(row.error.as_deref(), Some(E_FILE_INCOMPLETE));
        assert!(!row.notice_sent);
        assert_eq!(s.failed_notices().unwrap()[0].id, waiting);
        let row = s.get(held).unwrap().unwrap();
        assert_eq!(row.state, State::NeedsCheck);
        assert_eq!(row.error.as_deref(), Some(E_DISPATCH_HELD));
        assert_eq!(s.next_queued().unwrap().unwrap().id, plain);
    }

    #[test]
    fn reply_with_out_files_is_stored_once() {
        let (_d, path, mut s) = open_tmp();
        let id = reply_with_files(&mut s, "m1", 2);
        let row = s.get(id).unwrap().unwrap();
        assert_eq!(row.state, State::ReplyPending);
        assert_eq!(row.reply.as_deref(), Some("the answer"));
        let stored = row.out_files.clone().unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        let rec = s.recover_on_start().unwrap();
        assert_eq!(rec.reply_pending.len(), 1);
        assert_eq!(
            rec.reply_pending[0].out_files.as_deref(),
            Some(stored.as_str())
        );
        assert!(rec.interrupted_uploads.is_empty());
    }

    #[test]
    fn file_sent_and_skipped_update_json() {
        let (_d, _p, mut s) = open_tmp();
        let id = reply_with_files(&mut s, "m1", 3);
        s.mark_file_sent(id, 0).unwrap();
        s.mark_file_skipped(id, 2).unwrap();
        let f0 = out_file(&s, id, 0);
        assert_eq!(f0["sent"], true);
        assert_eq!(f0["skipped"], false);
        assert_eq!(f0["name"], "out0.txt");
        let f1 = out_file(&s, id, 1);
        assert_eq!(f1["sent"], false);
        assert_eq!(f1["skipped"], false);
        let f2 = out_file(&s, id, 2);
        assert_eq!(f2["sent"], false);
        assert_eq!(f2["skipped"], true);
        assert_eq!(s.mark_file_sent(id, 3).unwrap_err().code(), E_STORE_INVALID);
    }

    #[test]
    fn text_sent_survives_reopen() {
        let (_d, path, mut s) = open_tmp();
        let id = reply_with_files(&mut s, "m1", 0);
        assert!(!s.get(id).unwrap().unwrap().text_sent);
        s.mark_text_sent(id).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        let rec = s.recover_on_start().unwrap();
        assert!(rec.reply_pending[0].text_sent);
    }

    #[test]
    fn attempting_file_is_skipped_on_restart() {
        let (_d, path, mut s) = open_tmp();
        let id = reply_with_files(&mut s, "m1", 2);
        s.mark_file_attempting(id, 0).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        let rec = s.recover_on_start().unwrap();
        assert_eq!(rec.interrupted_uploads, vec![id]);
        let f0 = out_file(&s, id, 0);
        assert_eq!(f0["skipped"], true);
        assert_eq!(f0["attempting"], false);
        assert_eq!(f0["sent"], false);
        let f1 = out_file(&s, id, 1);
        assert_eq!(f1["skipped"], false);
        assert_eq!(f1["attempting"], false);
        assert_eq!(s.get(id).unwrap().unwrap().state, State::ReplyPending);
        assert!(s.recover_on_start().unwrap().interrupted_uploads.is_empty());
    }

    #[test]
    fn file_sent_clears_attempting() {
        let (_d, path, mut s) = open_tmp();
        let id = reply_with_files(&mut s, "m1", 1);
        s.mark_file_attempting(id, 0).unwrap();
        s.mark_file_sent(id, 0).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        assert!(s.recover_on_start().unwrap().interrupted_uploads.is_empty());
        let f0 = out_file(&s, id, 0);
        assert_eq!(f0["sent"], true);
        assert_eq!(f0["skipped"], false);
        assert_eq!(f0["attempting"], false);
    }

    #[test]
    fn cleared_attempting_is_not_skipped() {
        let (_d, path, mut s) = open_tmp();
        let id = reply_with_files(&mut s, "m1", 1);
        s.mark_file_attempting(id, 0).unwrap();
        s.clear_file_attempting(id, 0).unwrap();
        drop(s);
        let mut s = Store::open(&path).unwrap();
        assert!(s.recover_on_start().unwrap().interrupted_uploads.is_empty());
        let f0 = out_file(&s, id, 0);
        assert_eq!(f0["skipped"], false);
        assert_eq!(f0["attempting"], false);
    }

    #[test]
    fn attempting_increments_attempts() {
        let (_d, path, mut s) = open_tmp();
        let id = new_id(admit(&mut s, "m1", "k1"));
        s.mark(id, State::Processing, None).unwrap();
        // An entry written without the `attempts` field counts as 0.
        s.save_reply(
            id,
            "the answer",
            Some(r#"[{"path":"/work/a.txt","name":"a.txt","sent":false,"skipped":false}]"#),
        )
        .unwrap();
        for _ in 0..2 {
            s.mark_file_attempting(id, 0).unwrap();
            s.clear_file_attempting(id, 0).unwrap();
        }
        drop(s);
        let s = Store::open(&path).unwrap();
        let f0 = out_file(&s, id, 0);
        assert_eq!(f0["attempts"], 2);
        assert_eq!(f0["attempting"], false);
        s.mark_file_sent(id, 0).unwrap();
        let f0 = out_file(&s, id, 0);
        assert_eq!(f0["attempts"], 2);
        assert_eq!(f0["sent"], true);
    }

    #[test]
    fn database_file_is_owner_only() {
        let (_d, path, _s) = open_tmp();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    /// The slice-3 tables, written by hand so the slice-4 migration is tested against them.
    const SLICE3_SCHEMA: &str = "
        CREATE TABLE inbox(
            id INTEGER PRIMARY KEY,
            platform TEXT NOT NULL,
            message_id TEXT NOT NULL,
            conversation_key TEXT NOT NULL,
            author_id TEXT NOT NULL,
            text TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('queued','processing','reply_pending','done','failed','needs_check')),
            reply TEXT,
            notice_sent INTEGER NOT NULL DEFAULT 0,
            received_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            error TEXT,
            attachments TEXT,
            files_ready INTEGER NOT NULL DEFAULT 1,
            out_files TEXT,
            text_sent INTEGER NOT NULL DEFAULT 0,
            UNIQUE(platform, message_id)
        );
        CREATE TABLE conversations(
            conversation_key TEXT PRIMARY KEY,
            platform TEXT,
            channel_id TEXT NOT NULL,
            omp_session_id TEXT,
            created_at TEXT,
            last_activity_at TEXT
        );
        CREATE TABLE known_threads(
            platform TEXT,
            thread_id TEXT,
            PRIMARY KEY(platform, thread_id)
        );
    ";

    const K1: &str = "p:th:111";
    const K2: &str = "p:th:222";

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn state_of(s: &Store, id: i64) -> (State, Option<String>) {
        let row = s.get(id).unwrap().unwrap();
        (row.state, row.error)
    }

    fn claimed(s: &mut Store, mid: &str, key: &str) -> i64 {
        let id = new_id(admit(s, mid, key));
        assert!(s.claim(id).unwrap());
        id
    }

    fn new_lane(s: &mut Store, call: &str, key: &str) -> i64 {
        match s.start_lane(call, key, "task", None, None, 3).unwrap() {
            LaneStart::New(id) => id,
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn slice3_database_gains_lead_tables_and_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SLICE3_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state, reply, received_at, updated_at)
             VALUES ('test', 'm1', ?1, '111', 'hello', 'done', 'r', 't', 't')",
            [K1],
        )
        .unwrap();
        drop(conn);
        for _ in 0..2 {
            let s = Store::open(&path).unwrap();
            let cols = columns(&s.conn);
            for c in ["origin", "steered_into"] {
                assert!(cols.iter().any(|x| x == c), "missing column {c}");
            }
            let names = tables(&s.conn);
            for t in ["sessions", "lanes"] {
                assert!(names.iter().any(|x| x == t), "missing table {t}");
            }
            let row = s.get(1).unwrap().unwrap();
            assert_eq!(row.origin, "owner");
            assert_eq!(row.steered_into, None);
            assert_eq!(row.state, State::Done);
            assert_eq!(s.count().unwrap(), 1);
        }
    }

    /// The slice-4 `lanes` table (and the tables a slice-4 database has besides it), written
    /// by hand so the slice-5 migration is tested against it: no `done` column.
    const SLICE4_SCHEMA: &str = "
        CREATE TABLE inbox(
            id INTEGER PRIMARY KEY,
            platform TEXT NOT NULL,
            message_id TEXT NOT NULL,
            conversation_key TEXT NOT NULL,
            author_id TEXT NOT NULL,
            text TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('queued','processing','reply_pending','done','failed','needs_check')),
            reply TEXT,
            notice_sent INTEGER NOT NULL DEFAULT 0,
            received_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            error TEXT,
            attachments TEXT,
            files_ready INTEGER NOT NULL DEFAULT 1,
            out_files TEXT,
            text_sent INTEGER NOT NULL DEFAULT 0,
            origin TEXT NOT NULL DEFAULT 'owner',
            steered_into INTEGER,
            UNIQUE(platform, message_id)
        );
        CREATE TABLE conversations(
            conversation_key TEXT PRIMARY KEY,
            platform TEXT,
            channel_id TEXT NOT NULL,
            omp_session_id TEXT,
            created_at TEXT,
            last_activity_at TEXT
        );
        CREATE TABLE known_threads(
            platform TEXT,
            thread_id TEXT,
            PRIMARY KEY(platform, thread_id)
        );
        CREATE TABLE sessions(
            name TEXT PRIMARY KEY,
            omp_session_id TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE lanes(
            id INTEGER PRIMARY KEY,
            tool_call_id TEXT NOT NULL UNIQUE,
            conversation_key TEXT NOT NULL,
            task TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('running','idle','failed','needs_check')),
            error TEXT,
            prompts INTEGER NOT NULL DEFAULT 1,
            omp_session_id TEXT,
            notice_sent INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
    ";

    #[test]
    fn slice4_database_gains_lanes_done_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SLICE4_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO lanes(tool_call_id, conversation_key, task, state, error, prompts, omp_session_id, notice_sent, created_at, updated_at)
             VALUES ('call-old', ?1, 'old task', 'idle', 'E_X', 2, 'sess-9', 1, 't', 't')",
            [K1],
        )
        .unwrap();
        drop(conn);
        for _ in 0..2 {
            let s = Store::open(&path).unwrap();
            let cols: Vec<String> = s
                .conn
                .prepare("SELECT name FROM pragma_table_info('lanes')")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(cols.iter().any(|c| c == "done"), "missing column done");
            assert_eq!(
                s.lane(1).unwrap().unwrap(),
                LaneRow {
                    id: 1,
                    conversation_key: K1.to_string(),
                    task: "old task".to_string(),
                    state: LaneState::Idle,
                    error: Some("E_X".to_string()),
                    prompts: 2,
                    omp_session_id: Some("sess-9".to_string()),
                    notice_sent: true,
                    done: None,
                    repo: None,
                    closed: false,
                    close_reason: None,
                }
            );
        }
    }

    #[test]
    fn start_lane_stores_done_line() {
        let (_d, _p, mut s) = open_tmp();
        let a = match s
            .start_lane("call-a", K1, "task", Some("file a"), None, 3)
            .unwrap()
        {
            LaneStart::New(id) => id,
            other => panic!("expected New, got {other:?}"),
        };
        assert_eq!(s.lane(a).unwrap().unwrap().done.as_deref(), Some("file a"));
        let b = new_lane(&mut s, "call-b", K1);
        assert_eq!(s.lane(b).unwrap().unwrap().done, None);
        assert_eq!(
            s.start_lane("call-a", K1, "task", Some("file other"), None, 3)
                .unwrap(),
            LaneStart::Existing(a)
        );
        assert_eq!(s.lane(a).unwrap().unwrap().done.as_deref(), Some("file a"));
    }

    #[test]
    fn start_lane_stores_repo() {
        let (_d, _p, mut s) = open_tmp();
        assert_eq!(
            s.start_lane("call-a", K1, "task", Some("file a"), Some("/qa/repo"), 3)
                .unwrap(),
            LaneStart::New(1)
        );
        assert_eq!(
            s.lane(1).unwrap().unwrap().repo.as_deref(),
            Some("/qa/repo")
        );
        assert_eq!(
            s.start_lane("call-b", K1, "task", None, None, 3).unwrap(),
            LaneStart::New(2)
        );
        assert_eq!(s.lane(2).unwrap().unwrap().repo, None);
    }

    #[test]
    fn slice5_database_gains_lanes_repo_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SLICE4_SCHEMA).unwrap();
        conn.execute_batch("ALTER TABLE lanes ADD COLUMN done TEXT")
            .unwrap();
        conn.execute(
            "INSERT INTO lanes(tool_call_id, conversation_key, task, state, error, prompts, omp_session_id, notice_sent, created_at, updated_at, done)
             VALUES ('call-old', ?1, 'old task', 'idle', NULL, 1, NULL, 0, 't', 't', 'file a')",
            [K1],
        )
        .unwrap();
        drop(conn);
        for _ in 0..2 {
            let s = Store::open(&path).unwrap();
            let cols: Vec<String> = s
                .conn
                .prepare("SELECT name FROM pragma_table_info('lanes')")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(cols.iter().any(|c| c == "repo"), "missing column repo");
            let lane = s.lane(1).unwrap().unwrap();
            assert_eq!(lane.repo, None);
            assert_eq!(lane.done.as_deref(), Some("file a"));
        }
    }

    #[test]
    fn l2_database_gains_lanes_close_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SLICE4_SCHEMA).unwrap();
        conn.execute_batch("ALTER TABLE lanes ADD COLUMN done TEXT")
            .unwrap();
        conn.execute_batch("ALTER TABLE lanes ADD COLUMN repo TEXT")
            .unwrap();
        conn.execute(
            "INSERT INTO lanes(tool_call_id, conversation_key, task, state, error, prompts, omp_session_id, notice_sent, created_at, updated_at, done, repo)
             VALUES ('call-old', ?1, 'old task', 'idle', NULL, 1, NULL, 0, 't', 't', NULL, '/qa/repo')",
            [K1],
        )
        .unwrap();
        drop(conn);
        for _ in 0..2 {
            let s = Store::open(&path).unwrap();
            let cols: Vec<String> = s
                .conn
                .prepare("SELECT name FROM pragma_table_info('lanes')")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for c in ["closed_at", "close_reason"] {
                assert!(cols.iter().any(|x| x == c), "missing column {c}");
            }
            let lane = s.lane(1).unwrap().unwrap();
            assert!(!lane.closed);
            assert_eq!(lane.close_reason, None);
            assert_eq!(lane.repo.as_deref(), Some("/qa/repo"));
        }
    }

    #[test]
    fn session_id_roundtrip() {
        let (_d, path, s) = open_tmp();
        assert_eq!(s.session_id("lead").unwrap(), None);
        s.set_session_id("lead", "sess-1").unwrap();
        assert_eq!(s.session_id("lead").unwrap().as_deref(), Some("sess-1"));
        s.set_session_id("lead", "sess-2").unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.session_id("lead").unwrap().as_deref(), Some("sess-2"));
        assert_eq!(s.session_id("other").unwrap(), None);
    }

    #[test]
    fn claim_and_steer_are_exclusive() {
        let (_d, _p, mut s) = open_tmp();
        let m = claimed(&mut s, "m1", K1);
        let st = new_id(admit(&mut s, "m2", K1));
        assert!(s.steer_into(st, m).unwrap());
        assert!(!s.claim(st).unwrap());
        let row = s.get(st).unwrap().unwrap();
        assert_eq!(row.state, State::Processing);
        assert_eq!(row.steered_into, Some(m));
        let t = new_id(admit(&mut s, "m3", K2));
        assert!(s.claim(t).unwrap());
        assert!(!s.steer_into(t, m).unwrap());
        assert_eq!(s.get(t).unwrap().unwrap().steered_into, None);
    }

    #[test]
    fn steer_refused_while_older_row_of_conversation_queued() {
        let (_d, _p, mut s) = open_tmp();
        let m = claimed(&mut s, "m1", K1);
        let o = new_id(admit(&mut s, "m2", K1));
        let n = new_id(admit(&mut s, "m3", K1));
        let x = new_id(admit(&mut s, "m4", K2));
        assert!(!s.steer_into(n, m).unwrap());
        assert_eq!(state_of(&s, n).0, State::Queued);
        assert!(s.steer_into(x, m).unwrap());
        assert!(s.claim(o).unwrap());
        assert!(s.steer_into(n, m).unwrap());
        assert_eq!(state_of(&s, n).0, State::Processing);
    }

    #[test]
    fn steer_never_older_than_main() {
        let (_d, _p, mut s) = open_tmp();
        let e = new_id(admit(&mut s, "m1", K1));
        let m = new_id(admit(&mut s, "m2", K1));
        assert!(s.claim(m).unwrap());
        assert!(!s.steer_into(e, m).unwrap());
        let row = s.get(e).unwrap().unwrap();
        assert_eq!(row.state, State::Queued);
        assert_eq!(row.steered_into, None);
        assert!(s.claim(e).unwrap());
        let n = new_id(admit(&mut s, "m3", K1));
        assert!(s.steer_into(n, m).unwrap());
    }

    #[test]
    fn finish_group_marks_main_reply_pending_and_steered_done() {
        let (_d, _p, mut s) = open_tmp();
        let m = claimed(&mut s, "m1", K1);
        let st = new_id(admit(&mut s, "m2", K1));
        assert!(s.steer_into(st, m).unwrap());
        s.finish_group(m, "both", Some("[]"), Some(st), &TurnTrace::default())
            .unwrap();
        let main = s.get(m).unwrap().unwrap();
        assert_eq!(main.state, State::ReplyPending);
        assert_eq!(main.reply.as_deref(), Some("both"));
        assert_eq!(main.out_files.as_deref(), Some("[]"));
        let steered = s.get(st).unwrap().unwrap();
        assert_eq!(steered.state, State::Done);
        assert_eq!(steered.reply, None);
        // Without a steer only the main row changes.
        let solo = claimed(&mut s, "m3", K2);
        s.finish_group(solo, "one", None, None, &TurnTrace::default())
            .unwrap();
        assert_eq!(state_of(&s, solo).0, State::ReplyPending);
    }

    #[test]
    fn fail_group_marks_all_failed() {
        let (_d, _p, mut s) = open_tmp();
        let m = claimed(&mut s, "m1", K1);
        let st = new_id(admit(&mut s, "m2", K1));
        assert!(s.steer_into(st, m).unwrap());
        let other = new_id(admit(&mut s, "m3", K2));
        s.fail_group(&[m, st], "E_TEST", &TurnTrace::default())
            .unwrap();
        for id in [m, st] {
            assert_eq!(
                state_of(&s, id),
                (State::Failed, Some("E_TEST".to_string()))
            );
        }
        assert_eq!(state_of(&s, other), (State::Queued, None));
    }

    #[test]
    fn interrupt_group_holds_queued_rows_of_each_conversation() {
        let (_d, _p, mut s) = open_tmp();
        let m = claimed(&mut s, "m1", K1);
        let st = new_id(admit(&mut s, "m2", K1));
        assert!(s.steer_into(st, m).unwrap());
        let same = new_id(admit(&mut s, "m3", K1));
        let other = new_id(admit(&mut s, "m4", K2));
        s.interrupt_group(&[m, st], "E_CHILD_LOST", &TurnTrace::default())
            .unwrap();
        for id in [m, st] {
            assert_eq!(
                state_of(&s, id),
                (State::NeedsCheck, Some("E_CHILD_LOST".to_string()))
            );
        }
        assert_eq!(
            state_of(&s, same),
            (State::NeedsCheck, Some(E_DISPATCH_HELD.to_string()))
        );
        assert_eq!(state_of(&s, other), (State::Queued, None));
    }

    #[test]
    fn hold_skips_lane_result_rows() {
        for restart in [false, true] {
            let (_d, _p, mut s) = open_tmp();
            let m = claimed(&mut s, "m1", K1);
            let lane = new_lane(&mut s, "call-1", K1);
            assert!(s.finish_lane(lane, 1, "r", LaneState::Idle, None).unwrap());
            let l = s.next_queued().unwrap().unwrap().id;
            let q = new_id(admit(&mut s, "m2", K1));
            let main_error = if restart {
                s.recover_on_start().unwrap();
                "interrupted"
            } else {
                s.interrupt_group(&[m], "E_CHILD_LOST", &TurnTrace::default())
                    .unwrap();
                "E_CHILD_LOST"
            };
            assert_eq!(
                state_of(&s, m),
                (State::NeedsCheck, Some(main_error.to_string()))
            );
            assert_eq!(
                state_of(&s, q),
                (State::NeedsCheck, Some(E_DISPATCH_HELD.to_string()))
            );
            assert_eq!(state_of(&s, l), (State::Queued, None));
            assert_eq!(s.next_queued().unwrap().unwrap().id, l);
        }
    }

    #[test]
    fn start_lane_dedups_tool_call_and_caps_running() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert_eq!(
            s.start_lane("call-a", K1, "task", None, None, 3).unwrap(),
            LaneStart::Existing(a)
        );
        new_lane(&mut s, "call-b", K1);
        new_lane(&mut s, "call-c", K2);
        assert_eq!(
            s.start_lane("call-d", K1, "task", None, None, 3).unwrap(),
            LaneStart::Limit
        );
        assert_eq!(s.lanes().unwrap().len(), 3);
        assert!(s.finish_lane(a, 1, "done", LaneState::Idle, None).unwrap());
        assert!(matches!(
            s.start_lane("call-d", K1, "task", None, None, 3).unwrap(),
            LaneStart::New(_)
        ));
        let row = s.lane(a).unwrap().unwrap();
        assert_eq!(row.state, LaneState::Idle);
        assert_eq!(row.prompts, 1);
        assert_eq!(row.conversation_key, K1);
    }

    #[test]
    fn message_lane_refuses_busy_and_unknown_and_counts_seq() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert_eq!(s.message_lane(a, 3).unwrap(), LaneMessage::Busy);
        assert_eq!(s.message_lane(a + 100, 3).unwrap(), LaneMessage::Unknown);
        assert!(s.finish_lane(a, 1, "done", LaneState::Idle, None).unwrap());
        // At the cap an idle lane may not run again.
        new_lane(&mut s, "call-b", K1);
        assert_eq!(s.message_lane(a, 1).unwrap(), LaneMessage::Limit);
        assert_eq!(s.lane(a).unwrap().unwrap().state, LaneState::Idle);
        assert_eq!(s.message_lane(a, 3).unwrap(), LaneMessage::Seq(2));
        let row = s.lane(a).unwrap().unwrap();
        assert_eq!(row.state, LaneState::Running);
        assert_eq!(row.prompts, 2);
    }

    fn closed_at(s: &Store, id: i64) -> Option<String> {
        s.conn
            .query_row("SELECT closed_at FROM lanes WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[test]
    fn close_lane_refuses_running_and_unknown_and_keeps_the_first_reason() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert_eq!(s.close_lane(a, "x").unwrap(), LaneClose::Busy);
        assert!(!s.lane(a).unwrap().unwrap().closed);
        assert_eq!(s.close_lane(a + 100, "x").unwrap(), LaneClose::Unknown);
        assert!(s.finish_lane(a, 1, "r", LaneState::Idle, None).unwrap());
        assert_eq!(
            s.close_lane(a, "first").unwrap(),
            LaneClose::Closed { repo: None }
        );
        let row = s.lane(a).unwrap().unwrap();
        assert!(row.closed);
        assert_eq!(row.close_reason.as_deref(), Some("first"));
        assert_eq!(row.state, LaneState::Idle);
        let t1 = closed_at(&s, a);
        assert!(t1.is_some());
        assert_eq!(
            s.close_lane(a, "again").unwrap(),
            LaneClose::Closed { repo: None }
        );
        let row = s.lane(a).unwrap().unwrap();
        assert_eq!(row.close_reason.as_deref(), Some("first"));
        assert_eq!(closed_at(&s, a), t1);
        let b = match s
            .start_lane("call-b", K1, "task", Some("file ."), Some("/qa/repo"), 3)
            .unwrap()
        {
            LaneStart::New(id) => id,
            other => panic!("expected New, got {other:?}"),
        };
        assert!(
            s.finish_lane(b, 1, "r", LaneState::Failed, Some("E_X"))
                .unwrap()
        );
        assert_eq!(
            s.close_lane(b, "x").unwrap(),
            LaneClose::Closed {
                repo: Some("/qa/repo".into())
            }
        );
    }

    #[test]
    fn message_lane_refuses_a_closed_lane() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert!(s.finish_lane(a, 1, "r", LaneState::Idle, None).unwrap());
        assert_eq!(
            s.close_lane(a, "x").unwrap(),
            LaneClose::Closed { repo: None }
        );
        assert_eq!(s.message_lane(a, 3).unwrap(), LaneMessage::Closed);
        let row = s.lane(a).unwrap().unwrap();
        assert_eq!(row.state, LaneState::Idle);
        assert_eq!(row.prompts, 1);
    }

    #[test]
    fn finish_lane_inserts_one_result_row_once() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert!(
            s.finish_lane(a, 1, "result", LaneState::Idle, None)
                .unwrap()
        );
        assert_eq!(s.count().unwrap(), 1);
        let row = s.next_queued().unwrap().unwrap();
        assert_eq!(row.platform, "lane");
        assert_eq!(row.message_id, format!("{a}:1"));
        assert_eq!(row.origin, "lane");
        assert_eq!(row.author_id, "lane");
        assert_eq!(row.conversation_key, K1);
        assert_eq!(row.text, "result");
        assert!(row.files_ready);
        assert_eq!(s.lane(a).unwrap().unwrap().state, LaneState::Idle);
        // A forced duplicate changes neither the inbox nor the lane.
        assert!(
            !s.finish_lane(a, 1, "again", LaneState::Failed, Some("E_TEST"))
                .unwrap()
        );
        assert_eq!(s.count().unwrap(), 1);
        let lane = s.lane(a).unwrap().unwrap();
        assert_eq!(lane.state, LaneState::Idle);
        assert_eq!(lane.error, None);
    }

    #[test]
    fn finish_lane_does_not_touch_conversations() {
        let (_d, _p, mut s) = open_tmp();
        admit(&mut s, "m1", K1);
        let count = |s: &Store| -> i64 {
            s.conn
                .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
                .unwrap()
        };
        let before = count(&s);
        let a = new_lane(&mut s, "call-a", K2);
        assert!(s.finish_lane(a, 1, "r", LaneState::Idle, None).unwrap());
        assert_eq!(count(&s), before);
        assert_eq!(s.channel_of(K2).unwrap(), None);
    }

    type InboxCols = (String, String, String, String, String, String, String);

    fn inbox_cols(s: &Store) -> Vec<InboxCols> {
        let mut stmt = s
            .conn
            .prepare(
                "SELECT platform, message_id, conversation_key, author_id, text, state, origin
                 FROM inbox ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn inbox_row(mid: &str, text: &str) -> InboxCols {
        let own = |v: &str| v.to_owned();
        (
            own("lane"),
            own(mid),
            own(K1),
            own("lane"),
            own(text),
            own("queued"),
            own("lane"),
        )
    }

    #[test]
    fn stall_notice_queues_one_row_per_episode_and_keeps_the_lane_running() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert_eq!(a, 1);
        assert!(s.stall_notice(a, 1, 1, "n1").unwrap());
        assert!(!s.stall_notice(a, 1, 1, "n1b").unwrap());
        assert!(s.stall_notice(a, 1, 2, "n2").unwrap());
        assert_eq!(
            inbox_cols(&s),
            vec![
                inbox_row("1:1:stalled:1", "n1"),
                inbox_row("1:1:stalled:2", "n2")
            ]
        );
        assert_eq!(s.lane(a).unwrap().unwrap().state, LaneState::Running);
        assert!(s.finish_lane(a, 1, "r", LaneState::Idle, None).unwrap());
        let rows = inbox_cols(&s);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].1, "1:1");
    }

    #[test]
    fn stall_notice_skips_a_lane_that_is_not_running() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert!(s.finish_lane(a, 1, "r", LaneState::Idle, None).unwrap());
        assert!(!s.stall_notice(a, 1, 1, "n1").unwrap());
        assert_eq!(s.count().unwrap(), 1);
        assert!(s.stall_notice(a + 100, 1, 1, "n1").is_err());
    }

    #[test]
    fn restart_marks_running_lanes_needs_check_once() {
        let (_d, path, mut s) = open_tmp();
        let r = new_lane(&mut s, "call-r", K1);
        let t = new_lane(&mut s, "call-t", K1);
        let i = new_lane(&mut s, "call-i", K2);
        assert!(
            s.finish_lane(t, 1, "cut", LaneState::NeedsCheck, Some("E_TURN_TIMEOUT"))
                .unwrap()
        );
        assert!(s.finish_lane(i, 1, "ok", LaneState::Idle, None).unwrap());
        drop(s);
        let mut s = Store::open(&path).unwrap();
        s.recover_on_start().unwrap();
        let row = s.lane(r).unwrap().unwrap();
        assert_eq!(row.state, LaneState::NeedsCheck);
        assert_eq!(row.error.as_deref(), Some(E_LANE_INTERRUPTED));
        assert!(!row.notice_sent);
        let notices: Vec<i64> = s.lane_notices().unwrap().iter().map(|l| l.id).collect();
        assert_eq!(notices, vec![r]);
        s.mark_lane_notice_sent(r).unwrap();
        assert!(s.lane_notices().unwrap().is_empty());
        s.recover_on_start().unwrap();
        assert!(s.lane_notices().unwrap().is_empty());
        let timed_out = s.lane(t).unwrap().unwrap();
        assert_eq!(timed_out.state, LaneState::NeedsCheck);
        assert_eq!(timed_out.error.as_deref(), Some("E_TURN_TIMEOUT"));
        let idle = s.lane(i).unwrap().unwrap();
        assert_eq!(idle.state, LaneState::Idle);
        assert_eq!(idle.error, None);
    }

    #[test]
    fn restart_marks_steered_rows_needs_check() {
        let (_d, path, mut s) = open_tmp();
        let m = claimed(&mut s, "m1", K1);
        let st = new_id(admit(&mut s, "m2", K1));
        assert!(s.steer_into(st, m).unwrap());
        drop(s);
        let mut s = Store::open(&path).unwrap();
        let rec = s.recover_on_start().unwrap();
        let ids: Vec<i64> = rec.needs_check.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![m, st]);
        assert_eq!(state_of(&s, m).0, State::NeedsCheck);
        assert_eq!(state_of(&s, st).0, State::NeedsCheck);
    }

    #[test]
    fn lane_session_id_is_stored() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert_eq!(s.lane(a).unwrap().unwrap().omp_session_id, None);
        s.set_lane_session_id(a, "lane-sess").unwrap();
        assert_eq!(
            s.lane(a).unwrap().unwrap().omp_session_id.as_deref(),
            Some("lane-sess")
        );
        assert_eq!(
            s.set_lane_session_id(a + 100, "x").unwrap_err().code(),
            E_STORE_INVALID
        );
    }

    fn onboarding_rows(s: &Store) -> i64 {
        s.conn
            .query_row("SELECT COUNT(*) FROM onboarding", [], |r| r.get(0))
            .unwrap()
    }

    fn onboarding_row(state: OnboardingState, after_row: i64) -> Option<OnboardingRow> {
        Some(OnboardingRow { state, after_row })
    }

    #[test]
    fn onboarding_starts_restarts_and_finishes() {
        let (_d, _p, mut s) = open_tmp();
        assert_eq!(s.onboarding().unwrap(), None);
        assert_eq!(onboarding_rows(&s), 0);
        assert_eq!(s.start_onboarding().unwrap(), 0);
        assert_eq!(
            s.onboarding().unwrap(),
            onboarding_row(OnboardingState::Active, 0)
        );
        assert_eq!(onboarding_rows(&s), 1);
        assert!(s.finish_onboarding(OnboardingState::Done).unwrap());
        assert!(!s.finish_onboarding(OnboardingState::Done).unwrap());
        assert!(!s.finish_onboarding(OnboardingState::Skipped).unwrap());
        assert_eq!(
            s.onboarding().unwrap(),
            onboarding_row(OnboardingState::Done, 0)
        );
        assert_eq!(onboarding_rows(&s), 1);

        assert_eq!(new_id(admit(&mut s, "m1", K1)), 1);
        assert_eq!(s.start_onboarding().unwrap(), 1);
        assert_eq!(
            s.onboarding().unwrap(),
            onboarding_row(OnboardingState::Active, 1)
        );
        assert_eq!(onboarding_rows(&s), 1);
        assert!(s.finish_onboarding(OnboardingState::Skipped).unwrap());
        assert_eq!(
            s.onboarding().unwrap(),
            onboarding_row(OnboardingState::Skipped, 1)
        );
        assert_eq!(
            s.finish_onboarding(OnboardingState::Active)
                .unwrap_err()
                .code(),
            E_STORE_INVALID
        );
        assert_eq!(
            s.onboarding().unwrap(),
            onboarding_row(OnboardingState::Skipped, 1)
        );
        assert_eq!(onboarding_rows(&s), 1);
    }

    #[test]
    fn onboarding_turn_counts_owner_turns_after_the_start() {
        let (_d, _p, mut s) = open_tmp();
        assert_eq!(s.onboarding_turn(1).unwrap(), None);
        let r1 = claimed(&mut s, "m1", K1);
        assert_eq!(s.start_onboarding().unwrap(), r1);
        let r2 = claimed(&mut s, "m2", K1);
        let r3 = claimed(&mut s, "m3", K1);
        let queued = new_id(admit(&mut s, "m4", K2));
        let failed = claimed(&mut s, "m5", K2);
        s.mark(failed, State::Failed, Some("E_TEST")).unwrap();
        let steered = new_id(admit(&mut s, "m6", K1));
        assert!(s.steer_into(steered, r3).unwrap());
        let lane = new_lane(&mut s, "call-1", K1);
        assert!(
            s.finish_lane(lane, 1, "lane result", LaneState::Idle, None)
                .unwrap()
        );
        let lane_row: i64 = s
            .conn
            .query_row("SELECT id FROM inbox WHERE origin = 'lane'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let r8 = claimed(&mut s, "m8", K1);
        assert_eq!(
            [r1, r2, r3, queued, failed, steered, lane_row, r8],
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(state_of(&s, queued).0, State::Queued);

        assert_eq!(s.onboarding_turn(r3).unwrap(), Some(2));
        assert_eq!(s.onboarding_turn(r8).unwrap(), Some(3));
        assert_eq!(s.onboarding_turn(r1).unwrap(), Some(0));
        assert!(s.finish_onboarding(OnboardingState::Done).unwrap());
        assert_eq!(s.onboarding_turn(r8).unwrap(), None);
    }

    #[test]
    fn old_database_gets_the_onboarding_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SLICE4_SCHEMA).unwrap();
        assert!(!tables(&conn).contains(&"onboarding".to_string()));
        drop(conn);
        let s = Store::open(&path).unwrap();
        assert!(tables(&s.conn).contains(&"onboarding".to_string()));
        assert_eq!(s.start_onboarding().unwrap(), 0);
        assert_eq!(
            s.onboarding().unwrap(),
            onboarding_row(OnboardingState::Active, 0)
        );
    }

    fn pending_jobs(s: &Store, kind: JobKind) -> Vec<MemoryJob> {
        s.pending_memory_jobs(100)
            .unwrap()
            .jobs
            .into_iter()
            .filter(|job| job.kind == kind)
            .collect()
    }

    fn job_row(id: i64, origin: &str) -> JobRow {
        JobRow {
            id,
            origin: origin.into(),
        }
    }

    fn sample_trace() -> TurnTrace {
        TurnTrace {
            tools: vec![
                ToolUse::new("web_search", ToolSource::Web),
                ToolUse::new("task", ToolSource::Task),
            ],
            task: true,
            settled: true,
        }
    }

    /// Counts the memory wakes of `s`.
    fn count_wakes(s: &mut Store) -> Arc<std::sync::atomic::AtomicUsize> {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        s.set_memory_wake(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        wakes
    }

    fn wakes(n: &std::sync::atomic::AtomicUsize) -> usize {
        n.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A lane of `key` with its result `seq` stored (idle); returns the result's row id.
    fn lane_result(s: &mut Store, key: &str, seq: i64, text: &str) -> i64 {
        if s.lane(1).unwrap().is_none() {
            s.start_lane("call-1", key, "t", None, None, 3).unwrap();
        }
        assert!(s.finish_lane(1, seq, text, LaneState::Idle, None).unwrap());
        s.count().unwrap()
    }

    #[test]
    fn memory_tables_are_added_to_an_old_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SLICE4_SCHEMA).unwrap();
        drop(conn);
        let mut s = Store::open(&path).unwrap();
        let names = tables(&s.conn);
        for table in ["memory_jobs", "memory_log", "memory_cursor"] {
            assert!(names.contains(&table.to_string()), "{table}: {names:?}");
        }
        // The old rows take part: a turn of an old row leaves its job.
        let id = new_id(admit(&mut s, "m1", "k1"));
        assert!(s.claim(id).unwrap());
        s.finish_group(id, "r", None, None, &TurnTrace::default())
            .unwrap();
        assert_eq!(pending_jobs(&s, JobKind::Group).len(), 1);
    }

    #[test]
    fn memory_job_group_is_stored_with_the_reply() {
        let (_d, _p, mut s) = open_tmp();
        let wake = count_wakes(&mut s);
        let main = lane_result(&mut s, "k1", 1, "lane said");
        assert_eq!(wakes(&wake), 1, "the lane result's own job");
        let owner = new_id(admit(&mut s, "m1", "k1"));
        assert!(s.claim(main).unwrap());
        assert!(s.steer_into(owner, main).unwrap());
        s.finish_group(main, "both", None, Some(owner), &sample_trace())
            .unwrap();
        assert_eq!(wakes(&wake), 2);
        let jobs = pending_jobs(&s, JobKind::Group);
        assert_eq!(jobs.len(), 1);
        assert!(!jobs[0].partial);
        assert_eq!(
            jobs[0].payload,
            JobPayload::Group(GroupJob {
                rows: vec![job_row(main, "lane"), job_row(owner, "owner")],
                trace: Some(sample_trace()),
            })
        );
        let key: String = s
            .conn
            .query_row(
                "SELECT key FROM memory_jobs WHERE id = ?1",
                [jobs[0].id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(key, main.to_string());
    }

    #[test]
    fn memory_job_group_is_partial_on_fail_and_interrupt() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_id(admit(&mut s, "m1", "k1"));
        let b = new_id(admit(&mut s, "m2", "k2"));
        assert!(s.claim(a).unwrap());
        assert!(s.claim(b).unwrap());
        s.fail_group(&[a], "E_TEST", &sample_trace()).unwrap();
        s.interrupt_group(&[b], "E_CHILD_LOST", &TurnTrace::default())
            .unwrap();
        let jobs = pending_jobs(&s, JobKind::Group);
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|job| job.partial));
        assert_eq!(
            jobs.iter()
                .map(|job| job.payload.clone())
                .collect::<Vec<_>>(),
            [
                JobPayload::Group(GroupJob {
                    rows: vec![job_row(a, "owner")],
                    trace: Some(sample_trace()),
                }),
                JobPayload::Group(GroupJob {
                    rows: vec![job_row(b, "owner")],
                    trace: Some(TurnTrace::default()),
                }),
            ]
        );
    }

    #[test]
    fn memory_job_rolls_back_with_its_transaction() {
        let (_d, _p, mut s) = open_tmp();
        let wake = count_wakes(&mut s);
        let m = new_id(admit(&mut s, "m1", "k1"));
        assert!(s.claim(m).unwrap());
        assert!(
            s.finish_group(m, "r", None, Some(9999), &sample_trace())
                .is_err()
        );
        let row = s.get(m).unwrap().unwrap();
        assert_eq!(row.state, State::Processing);
        assert_eq!(row.reply, None);
        assert!(s.pending_memory_jobs(100).unwrap().jobs.is_empty());
        assert!(s.fail_group(&[m, 9999], "E_TEST", &sample_trace()).is_err());
        assert_eq!(s.get(m).unwrap().unwrap().state, State::Processing);
        assert!(s.pending_memory_jobs(100).unwrap().jobs.is_empty());
        assert_eq!(wakes(&wake), 0);
    }

    #[test]
    fn memory_job_lane_is_stored_once_per_result() {
        let (_d, _p, mut s) = open_tmp();
        let row = lane_result(&mut s, "k1", 1, "done");
        assert!(!s.finish_lane(1, 1, "again", LaneState::Idle, None).unwrap());
        assert!(
            s.finish_lane(1, 2, "broke", LaneState::Failed, Some("E_TEST"))
                .unwrap()
        );
        let jobs = pending_jobs(&s, JobKind::Lane);
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].payload,
            JobPayload::Lane(LaneJob {
                row,
                lane: 1,
                seq: 1
            })
        );
        assert_eq!(s.get(row).unwrap().unwrap().message_id, "1:1");
    }

    #[test]
    fn memory_job_recovery_groups_the_interrupted_rows() {
        let (_d, path, mut s) = open_tmp();
        let a = new_id(admit(&mut s, "m1", "k1"));
        let b = new_id(admit(&mut s, "m2", "k1"));
        let c = new_id(admit(&mut s, "m3", "k2"));
        assert!(s.claim(a).unwrap());
        assert!(s.steer_into(b, a).unwrap());
        assert!(s.claim(c).unwrap());
        drop(s);
        let mut s = Store::open(&path).unwrap();
        s.recover_on_start().unwrap();
        let jobs = pending_jobs(&s, JobKind::Group);
        assert_eq!(
            jobs.iter()
                .map(|job| (job.partial, job.payload.clone()))
                .collect::<Vec<_>>(),
            [
                (
                    true,
                    JobPayload::Group(GroupJob {
                        rows: vec![job_row(a, "owner"), job_row(b, "owner")],
                        trace: None,
                    })
                ),
                (
                    true,
                    JobPayload::Group(GroupJob {
                        rows: vec![job_row(c, "owner")],
                        trace: None,
                    })
                ),
            ]
        );
        // A second start finds nothing processing and adds nothing.
        s.recover_on_start().unwrap();
        assert_eq!(s.pending_memory_jobs(100).unwrap().jobs.len(), 2);
    }

    #[test]
    fn memory_job_scan_is_stored_on_the_first_lane_close() {
        let (_d, _p, mut s) = open_tmp();
        let wake = count_wakes(&mut s);
        lane_result(&mut s, "k1", 1, "done");
        assert!(matches!(
            s.close_lane(1, "qa").unwrap(),
            LaneClose::Closed { .. }
        ));
        assert!(matches!(
            s.close_lane(1, "again").unwrap(),
            LaneClose::Closed { .. }
        ));
        let jobs = pending_jobs(&s, JobKind::Scan);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].payload, JobPayload::Scan(ScanJob { lane: 1 }));
        assert_eq!(wakes(&wake), 2, "the lane result and the first close");
        assert!(matches!(s.close_lane(7, "qa").unwrap(), LaneClose::Unknown));
        assert_eq!(pending_jobs(&s, JobKind::Scan).len(), 1);
    }

    #[test]
    fn an_unreadable_memory_job_is_returned_apart() {
        let (_d, _p, mut s) = open_tmp();
        let job = |n: &str| SubagentJob {
            path: format!("/s/{n}.md"),
            session: "lead".into(),
            task: n.into(),
            sha256: n.into(),
        };
        for n in ["a", "b", "c"] {
            assert!(s.note_artifact(&job(n), 1, 1, 100).unwrap());
        }
        s.conn
            .execute(
                "UPDATE memory_jobs SET payload = 'not json' WHERE id = 1",
                [],
            )
            .unwrap();
        s.conn
            .execute("UPDATE memory_jobs SET kind = 'scan' WHERE id = 2", [])
            .unwrap();
        let pending = s.pending_memory_jobs(100).unwrap();
        assert_eq!(pending.unreadable, [1, 2]);
        assert_eq!(pending.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn memory_cursor_queues_a_new_or_changed_file_once() {
        let (_d, _p, mut s) = open_tmp();
        let job = |sha: &str| SubagentJob {
            path: "/s/lead/x/Explore_2.md".into(),
            session: "lead".into(),
            task: "Explore_2".into(),
            sha256: sha.into(),
        };
        let path = "/s/lead/x/Explore_2.md";
        assert!(!s.artifact_unchanged(path, 10, 7).unwrap());
        assert!(s.note_artifact(&job("aa"), 10, 7, 100).unwrap());
        assert!(s.artifact_unchanged(path, 10, 7).unwrap());
        // Touched, same content: no job, but the new mtime is kept so the next scan skips it.
        assert!(!s.note_artifact(&job("aa"), 10, 8, 200).unwrap());
        assert!(s.artifact_unchanged(path, 10, 8).unwrap());
        assert_eq!(pending_jobs(&s, JobKind::Subagent).len(), 1);
        assert!(s.note_artifact(&job("bb"), 12, 9, 300).unwrap());
        let jobs = pending_jobs(&s, JobKind::Subagent);
        assert_eq!(
            jobs.iter()
                .map(|j| (j.created_at, j.payload.clone()))
                .collect::<Vec<_>>(),
            [
                (100, JobPayload::Subagent(job("aa"))),
                (300, JobPayload::Subagent(job("bb")))
            ]
        );
        let cursor: (i64, String, i64) = s
            .conn
            .query_row("SELECT size, sha256, seen_at FROM memory_cursor", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(cursor, (12, "bb".into(), 300));
    }

    #[test]
    fn memory_job_payload_holds_no_message_text() {
        const SENTINEL: &str = "SENTINEL_7c1e";
        let (_d, path, mut s) = open_tmp();
        let text = format!("remember {SENTINEL}");
        let lane = lane_result(&mut s, "k1", 1, &text);
        let a = new_id(s.admit("test", "m1", "k1", "c", "111", &text).unwrap());
        assert!(s.claim(lane).unwrap());
        assert!(s.steer_into(a, lane).unwrap());
        s.finish_group(lane, &text, None, Some(a), &sample_trace())
            .unwrap();
        let b = new_id(s.admit("test", "m2", "k2", "c", "111", &text).unwrap());
        assert!(s.claim(b).unwrap());
        drop(s);
        let mut s = Store::open(&path).unwrap();
        s.recover_on_start().unwrap();
        s.close_lane(1, "qa").unwrap();
        let payloads: Vec<String> = {
            let mut stmt = s.conn.prepare("SELECT payload FROM memory_jobs").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(payloads.len(), 4, "lane, group, recovered group, scan");
        for payload in payloads {
            assert!(!payload.contains(SENTINEL), "{payload}");
        }
    }

    #[test]
    fn memory_job_settle_fail_expire_and_prune() {
        let (_d, _p, mut s) = open_tmp();
        let job = |n: &str| SubagentJob {
            path: format!("/s/{n}.md"),
            session: "lead".into(),
            task: n.into(),
            sha256: n.into(),
        };
        for (n, at) in [("a", 100), ("b", 100), ("c", 100), ("d", 900)] {
            assert!(s.note_artifact(&job(n), 1, 1, at).unwrap());
        }
        let m = new_id(admit(&mut s, "m1", "k1"));
        assert!(s.claim(m).unwrap());
        s.fail_group(&[m], "E_TEST", &TurnTrace::default()).unwrap();
        let ids: Vec<i64> = s
            .pending_memory_jobs(100)
            .unwrap()
            .jobs
            .iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(ids.len(), 5);
        assert_eq!(
            s.memory_backlog().unwrap(),
            Backlog {
                groups: 1,
                other: 4
            }
        );

        s.settle_memory_jobs(
            &[Settled {
                id: ids[0],
                end: JobEnd::Done,
                level: LogLevel::Info,
                code: "M_JOB_DONE",
                omp_version: "18.22.2".into(),
                jev_model: Some("jev-1".into()),
                detail: Some("kind=subagent saved=0".into()),
            }],
            1000,
        )
        .unwrap();
        assert!(
            s.memory_logged("M_JOB_DONE", "kind=subagent saved=0")
                .unwrap()
        );
        let (state, payload): (String, Option<String>) = s
            .conn
            .query_row(
                "SELECT state, payload FROM memory_jobs WHERE id = ?1",
                [ids[0]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), payload), ("done", None));

        assert!(
            !s.fail_memory_job_attempt(ids[1], "E_TEST", "v", 1000)
                .unwrap()
        );
        assert!(
            !s.fail_memory_job_attempt(ids[1], "E_TEST", "v", 1000)
                .unwrap()
        );
        assert!(
            s.fail_memory_job_attempt(ids[1], "E_TEST", "v", 1000)
                .unwrap()
        );
        assert!(
            !s.fail_memory_job_attempt(ids[1], "E_TEST", "v", 1000)
                .unwrap()
        );
        assert!(s.memory_logged("E_TEST", "attempts=3").unwrap());

        // c (created at 100) expires, d (900) and the group job do not.
        assert_eq!(s.expire_memory_jobs(1000, 500, "v").unwrap(), 1);
        assert_eq!(
            s.memory_backlog().unwrap(),
            Backlog {
                groups: 1,
                other: 1
            }
        );
        assert!(s.memory_logged(M_JOB_EXPIRED, "kind=subagent").unwrap());

        s.memory_log(&LogRow {
            at: 5000,
            level: LogLevel::Warn,
            code: "W_OMP_VERSION",
            job_id: None,
            omp_version: None,
            jev_model: None,
            detail: Some("18.23.0".into()),
        })
        .unwrap();
        s.prune_memory(1000 + MEMORY_KEEP_SECONDS + 1).unwrap();
        let count = |sql: &str| -> i64 { s.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count("SELECT COUNT(*) FROM memory_jobs"),
            2,
            "pending jobs stay"
        );
        assert_eq!(count("SELECT COUNT(*) FROM memory_log"), 1);
        assert!(s.memory_logged("W_OMP_VERSION", "18.23.0").unwrap());
        assert_eq!(count("SELECT COUNT(*) FROM memory_cursor"), 4);
    }

    #[test]
    fn onboarding_state_is_checked() {
        let (_d, _p, s) = open_tmp();
        assert!(
            s.conn
                .execute(
                    "INSERT INTO onboarding VALUES (1, 'later', 0, 'x', NULL)",
                    []
                )
                .is_err()
        );
        assert!(
            s.conn
                .execute(
                    "INSERT INTO onboarding VALUES (2, 'active', 0, 'x', NULL)",
                    []
                )
                .is_err()
        );
        assert_eq!(onboarding_rows(&s), 0);
    }

    #[test]
    fn an_old_memory_notices_table_is_dropped_and_its_lines_never_reach_a_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        {
            let s = Store::open(&path).unwrap();
            s.conn
                .execute_batch(
                    "CREATE TABLE memory_notices(
                         id INTEGER PRIMARY KEY,
                         conversation_key TEXT NOT NULL,
                         text TEXT NOT NULL,
                         created_at INTEGER NOT NULL
                     );
                     INSERT INTO memory_notices(conversation_key, text, created_at)
                     VALUES ('k:a', '[memory] saved m-1 to core/owner', 100);",
                )
                .unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        let left: bool = s
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'memory_notices')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!left);
        let id = match s.admit("discord", "m1", "k:a", "ch", "111", "hi").unwrap() {
            Admitted::New(id) => id,
            Admitted::Duplicate => panic!("new row"),
        };
        s.finish_group(id, "the reply", None, None, &TurnTrace::default())
            .unwrap();
        assert_eq!(
            s.get(id).unwrap().unwrap().reply.as_deref(),
            Some("the reply")
        );
    }
}
