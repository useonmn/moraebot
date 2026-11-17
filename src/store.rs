//! Durable SQLite inbox: store-first admission, global FIFO, restart recovery.
//!
//! One connection, `locking_mode=EXCLUSIVE` (single instance), WAL, `synchronous=FULL`.
//! Because of the exclusive lock, nothing else may open the file while a `Store` is
//! alive: read everything through this API.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{
    params, Connection, ErrorCode, OptionalExtension, Row as SqlRow, TransactionBehavior,
};
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
}

/// What one memory commit records besides its changes.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCommit {
    pub writer: String,
    pub reason: String,
    pub turn_row: Option<i64>,
    pub conversation_key: Option<String>,
    pub claim_json: Option<String>,
}

/// A stored memory commit with its id, timestamp and applied flag.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCommitRow {
    pub id: i64,
    pub at: String,
    pub commit: MemoryCommit,
    pub applied: bool,
}

/// One changed memory path: the path and its new full text, `None` when deleted.
pub type MemoryChange = (String, Option<String>);

pub struct Store {
    conn: Connection,
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
    done TEXT
);
CREATE TABLE IF NOT EXISTS memory_commits(
    id INTEGER PRIMARY KEY,
    at TEXT NOT NULL,
    writer TEXT NOT NULL,
    reason TEXT NOT NULL,
    turn_row INTEGER,
    conversation_key TEXT,
    claim_json TEXT,
    applied INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS memory_snapshots(
    commit_id INTEGER NOT NULL REFERENCES memory_commits(id),
    path TEXT NOT NULL,
    body TEXT,
    PRIMARY KEY(commit_id, path)
);
CREATE INDEX IF NOT EXISTS memory_snapshots_by_path ON memory_snapshots(path, commit_id);
";

/// Columns added after slice 2 as `(table, column, declaration)`; `Store::open` adds the
/// missing ones to an older database.
const ADDED_COLUMNS: [(&str, &str, &str); 7] = [
    ("inbox", "attachments", "TEXT"),
    ("inbox", "files_ready", "INTEGER NOT NULL DEFAULT 1"),
    ("inbox", "out_files", "TEXT"),
    ("inbox", "text_sent", "INTEGER NOT NULL DEFAULT 0"),
    ("inbox", "origin", "TEXT NOT NULL DEFAULT 'owner'"),
    ("inbox", "steered_into", "INTEGER"),
    ("lanes", "done", "TEXT"),
];

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// Adds every missing column of [`ADDED_COLUMNS`] in one transaction.
fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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

const LANE_COLS: &str =
    "id, conversation_key, task, state, error, prompts, omp_session_id, notice_sent, done";

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
        Ok(Self { conn })
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
    /// a restart.
    pub fn interrupt_group(&mut self, ids: &[i64], code: &str) -> Result<(), StoreError> {
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
        tx.commit()?;
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
    /// [`Store::save_reply`]) and marks the steered row, if any, `done`, in one transaction.
    pub fn finish_group(
        &mut self,
        main_id: i64,
        reply: &str,
        out_files: Option<&str>,
        steered: Option<i64>,
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
        tx.commit()?;
        Ok(())
    }

    /// Marks every row of a turn group `failed` with `code`, in one transaction.
    pub fn fail_group(&mut self, ids: &[i64], code: &str) -> Result<(), StoreError> {
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
        tx.commit()?;
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
    /// returned for re-sending.
    pub fn recover_on_start(&mut self) -> Result<Recovered, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids: Vec<i64> = {
            let mut stmt =
                tx.prepare("SELECT id FROM inbox WHERE state = 'processing' ORDER BY id")?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
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
                "INSERT INTO lanes(tool_call_id, conversation_key, task, done, state, prompts, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', 1, {NOW}, {NOW})"
            ),
            params![tool_call_id, conversation_key, task, done],
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
        let state: Option<String> = tx
            .query_row("SELECT state FROM lanes WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        match state.as_deref() {
            None => return Ok(LaneMessage::Unknown),
            Some("running") => return Ok(LaneMessage::Busy),
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
    /// and sets the lane's state, in one transaction. A second call for the same `seq`
    /// changes nothing and returns false.
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
        tx.execute(
            &format!("UPDATE lanes SET state = ?2, error = ?3, updated_at = {NOW} WHERE id = ?1"),
            params![id, state.as_str(), error],
        )?;
        tx.commit()?;
        Ok(true)
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

    /// Records a memory commit (`applied = 0`) and one snapshot per change in one
    /// transaction. Returns the commit id.
    pub fn record_memory_commit(
        &mut self,
        commit: &MemoryCommit,
        changes: &[MemoryChange],
    ) -> Result<i64, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            &format!(
                "INSERT INTO memory_commits(at, writer, reason, turn_row, conversation_key, claim_json)
                 VALUES ({NOW}, ?1, ?2, ?3, ?4, ?5)"
            ),
            params![
                commit.writer,
                commit.reason,
                commit.turn_row,
                commit.conversation_key,
                commit.claim_json
            ],
        )?;
        let id = tx.last_insert_rowid();
        for (path, body) in changes {
            tx.execute(
                "INSERT INTO memory_snapshots(commit_id, path, body) VALUES (?1, ?2, ?3)",
                params![id, path, body],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Marks a memory commit as written to the working files.
    pub fn mark_memory_applied(&self, id: i64) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute("UPDATE memory_commits SET applied = 1 WHERE id = ?1", [id])?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such memory commit"));
        }
        Ok(())
    }

    /// Memory commits not yet written to the working files, oldest first, each with its
    /// snapshots ordered by path.
    pub fn unapplied_memory_commits(&self) -> Result<Vec<(i64, Vec<MemoryChange>)>, StoreError> {
        let ids: Vec<i64> = self
            .conn
            .prepare("SELECT id FROM memory_commits WHERE applied = 0 ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        ids.into_iter()
            .map(|id| Ok((id, self.memory_snapshots(id)?)))
            .collect()
    }

    /// For every path ever recorded, its snapshot from the highest commit id (`None` when
    /// that commit deleted it).
    pub fn memory_latest(&self) -> Result<BTreeMap<String, Option<String>>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.path, s.body FROM memory_snapshots s
             WHERE s.commit_id = (SELECT MAX(commit_id) FROM memory_snapshots WHERE path = s.path)",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every memory commit, oldest first.
    pub fn memory_commits(&self) -> Result<Vec<MemoryCommitRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, at, writer, reason, turn_row, conversation_key, claim_json, applied
             FROM memory_commits ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MemoryCommitRow {
                id: r.get(0)?,
                at: r.get(1)?,
                commit: MemoryCommit {
                    writer: r.get(2)?,
                    reason: r.get(3)?,
                    turn_row: r.get(4)?,
                    conversation_key: r.get(5)?,
                    claim_json: r.get(6)?,
                },
                applied: r.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The snapshots of one memory commit, ordered by path.
    pub fn memory_snapshots(&self, commit_id: i64) -> Result<Vec<MemoryChange>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, body FROM memory_snapshots WHERE commit_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([commit_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
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
        assert!(s
            .admit("test", "m1", "k1", "chan1", "111", "hello")
            .is_err());
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
        s.interrupt_group(&[a], "E_TEST").unwrap();
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
        s.interrupt_group(&[a], "E_TEST").unwrap();
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
        s.interrupt_group(&[a], "E_TEST").unwrap();
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
        match s.start_lane(call, key, "task", None, 3).unwrap() {
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
                }
            );
        }
    }

    fn mem_commit(writer: &str) -> MemoryCommit {
        MemoryCommit {
            writer: writer.to_string(),
            reason: "r".to_string(),
            turn_row: None,
            conversation_key: None,
            claim_json: None,
        }
    }

    fn change(path: &str, body: Option<&str>) -> MemoryChange {
        (path.to_string(), body.map(str::to_string))
    }

    fn memory_objects(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT name FROM sqlite_master WHERE name LIKE 'memory_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    const MEMORY_OBJECTS: [&str; 3] = [
        "memory_commits",
        "memory_snapshots",
        "memory_snapshots_by_path",
    ];

    #[test]
    fn memory_tables_exist_after_open() {
        let (_d, _p, s) = open_tmp();
        assert_eq!(memory_objects(&s.conn), MEMORY_OBJECTS);
    }

    #[test]
    fn memory_tables_are_added_to_an_old_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SLICE4_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state, received_at, updated_at)
             VALUES ('test', 'old', ?1, '111', 'hi', 'done', 't', 't')",
            [K1],
        )
        .unwrap();
        assert!(memory_objects(&conn).is_empty());
        drop(conn);
        let s = Store::open(&path).unwrap();
        assert_eq!(memory_objects(&s.conn), MEMORY_OBJECTS);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn memory_commit_round_trips() {
        let (_d, _p, mut s) = open_tmp();
        let commit = MemoryCommit {
            writer: "lead".to_string(),
            reason: "why".to_string(),
            turn_row: Some(7),
            conversation_key: Some(K1.to_string()),
            claim_json: Some("{}".to_string()),
        };
        let id = s
            .record_memory_commit(
                &commit,
                &[
                    change("system/b.md", Some("text b")),
                    change("notes/a.md", None),
                ],
            )
            .unwrap();
        let rows = s.memory_commits().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].commit, commit);
        assert!(!rows[0].applied);
        assert!(!rows[0].at.is_empty());
        assert_eq!(
            s.memory_snapshots(id).unwrap(),
            vec![
                change("notes/a.md", None),
                change("system/b.md", Some("text b")),
            ]
        );
    }

    #[test]
    fn memory_record_is_one_transaction() {
        let (_d, _p, mut s) = open_tmp();
        s.conn
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON memory_snapshots
                 BEGIN SELECT RAISE(ABORT, 'forced'); END;",
            )
            .unwrap();
        assert!(
            s.record_memory_commit(&mem_commit("lead"), &[change("notes/a.md", Some("x"))])
                .is_err()
        );
        assert!(s.memory_commits().unwrap().is_empty());
    }

    #[test]
    fn latest_snapshot_per_path_wins() {
        let (_d, _p, mut s) = open_tmp();
        s.record_memory_commit(
            &mem_commit("import"),
            &[change("a.md", Some("a1")), change("b.md", Some("b1"))],
        )
        .unwrap();
        s.record_memory_commit(
            &mem_commit("lead"),
            &[change("a.md", Some("a2")), change("c.md", Some("c2"))],
        )
        .unwrap();
        s.record_memory_commit(&mem_commit("lead"), &[change("b.md", None)])
            .unwrap();
        let latest = s.memory_latest().unwrap();
        assert_eq!(
            latest.into_iter().collect::<Vec<_>>(),
            vec![
                change("a.md", Some("a2")),
                change("b.md", None),
                change("c.md", Some("c2")),
            ]
        );
    }

    #[test]
    fn unapplied_lists_only_unapplied_in_order() {
        let (_d, _p, mut s) = open_tmp();
        let ids: Vec<i64> = ["a", "b", "c"]
            .iter()
            .map(|p| {
                s.record_memory_commit(
                    &mem_commit("lead"),
                    &[
                        change(&format!("{p}2.md"), Some("2")),
                        change(&format!("{p}1.md"), None),
                    ],
                )
                .unwrap()
            })
            .collect();
        s.mark_memory_applied(ids[1]).unwrap();
        let pending = s.unapplied_memory_commits().unwrap();
        assert_eq!(
            pending.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [ids[0], ids[2]]
        );
        assert_eq!(
            pending[0].1,
            vec![change("a1.md", None), change("a2.md", Some("2"))]
        );
        let err = s.mark_memory_applied(ids[2] + 100).unwrap_err();
        assert_eq!(err.code(), E_STORE_INVALID);
    }

    #[test]
    fn start_lane_stores_done_line() {
        let (_d, _p, mut s) = open_tmp();
        let a = match s
            .start_lane("call-a", K1, "task", Some("file a"), 3)
            .unwrap()
        {
            LaneStart::New(id) => id,
            other => panic!("expected New, got {other:?}"),
        };
        assert_eq!(s.lane(a).unwrap().unwrap().done.as_deref(), Some("file a"));
        let b = new_lane(&mut s, "call-b", K1);
        assert_eq!(s.lane(b).unwrap().unwrap().done, None);
        assert_eq!(
            s.start_lane("call-a", K1, "task", Some("file other"), 3)
                .unwrap(),
            LaneStart::Existing(a)
        );
        assert_eq!(s.lane(a).unwrap().unwrap().done.as_deref(), Some("file a"));
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
        s.finish_group(m, "both", Some("[]"), Some(st)).unwrap();
        let main = s.get(m).unwrap().unwrap();
        assert_eq!(main.state, State::ReplyPending);
        assert_eq!(main.reply.as_deref(), Some("both"));
        assert_eq!(main.out_files.as_deref(), Some("[]"));
        let steered = s.get(st).unwrap().unwrap();
        assert_eq!(steered.state, State::Done);
        assert_eq!(steered.reply, None);
        // Without a steer only the main row changes.
        let solo = claimed(&mut s, "m3", K2);
        s.finish_group(solo, "one", None, None).unwrap();
        assert_eq!(state_of(&s, solo).0, State::ReplyPending);
    }

    #[test]
    fn fail_group_marks_all_failed() {
        let (_d, _p, mut s) = open_tmp();
        let m = claimed(&mut s, "m1", K1);
        let st = new_id(admit(&mut s, "m2", K1));
        assert!(s.steer_into(st, m).unwrap());
        let other = new_id(admit(&mut s, "m3", K2));
        s.fail_group(&[m, st], "E_TEST").unwrap();
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
        s.interrupt_group(&[m, st], "E_CHILD_LOST").unwrap();
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
                s.interrupt_group(&[m], "E_CHILD_LOST").unwrap();
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
            s.start_lane("call-a", K1, "task", None, 3).unwrap(),
            LaneStart::Existing(a)
        );
        new_lane(&mut s, "call-b", K1);
        new_lane(&mut s, "call-c", K2);
        assert_eq!(
            s.start_lane("call-d", K1, "task", None, 3).unwrap(),
            LaneStart::Limit
        );
        assert_eq!(s.lanes().unwrap().len(), 3);
        assert!(s.finish_lane(a, 1, "done", LaneState::Idle, None).unwrap());
        assert!(matches!(
            s.start_lane("call-d", K1, "task", None, 3).unwrap(),
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

    #[test]
    fn finish_lane_inserts_one_result_row_once() {
        let (_d, _p, mut s) = open_tmp();
        let a = new_lane(&mut s, "call-a", K1);
        assert!(s
            .finish_lane(a, 1, "result", LaneState::Idle, None)
            .unwrap());
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
        assert!(!s
            .finish_lane(a, 1, "again", LaneState::Failed, Some("E_TEST"))
            .unwrap());
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

    #[test]
    fn restart_marks_running_lanes_needs_check_once() {
        let (_d, path, mut s) = open_tmp();
        let r = new_lane(&mut s, "call-r", K1);
        let t = new_lane(&mut s, "call-t", K1);
        let i = new_lane(&mut s, "call-i", K2);
        assert!(s
            .finish_lane(t, 1, "cut", LaneState::NeedsCheck, Some("E_TURN_TIMEOUT"))
            .unwrap());
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
}
