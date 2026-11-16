//! Durable SQLite inbox: store-first admission, global FIFO, restart recovery.
//!
//! One connection, `locking_mode=EXCLUSIVE` (single instance), WAL, `synchronous=FULL`.
//! Because of the exclusive lock, nothing else may open the file while a `Store` is
//! alive: read everything through this API.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{
    params, Connection, ErrorCode, OptionalExtension, Row as SqlRow, TransactionBehavior,
};

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
}

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
";

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// Holds the queued rows of the conversations selected by the appended subquery: they
/// become `needs_check` / [`E_DISPATCH_HELD`] with their notice still to be posted.
fn hold_queued_sql(conversations: &str) -> String {
    format!(
        "UPDATE inbox SET state = 'needs_check', error = '{E_DISPATCH_HELD}', updated_at = {NOW}
         WHERE state = 'queued' AND conversation_key IN ({conversations})"
    )
}
const COLS: &str =
    "id, platform, message_id, conversation_key, author_id, text, state, reply, notice_sent, error";

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
        if conversation_key.is_empty() {
            return Err(StoreError::Invalid("conversation_key must not be empty"));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            &format!(
                "INSERT INTO inbox(platform, message_id, conversation_key, author_id, text, state, received_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', {NOW}, {NOW})
                 ON CONFLICT(platform, message_id) DO NOTHING"
            ),
            params![platform, message_id, conversation_key, author_id, text],
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
                &format!("SELECT {COLS} FROM inbox WHERE state = 'queued' ORDER BY id LIMIT 1"),
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

    /// Persists the reply text and moves the row to `reply_pending` in one statement.
    pub fn save_reply(&self, id: i64, reply: &str) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            &format!(
                "UPDATE inbox SET reply = ?2, state = 'reply_pending', error = NULL, updated_at = {NOW} WHERE id = ?1"
            ),
            params![id, reply],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such inbox row"));
        }
        Ok(())
    }

    /// Marks an interrupted row `needs_check` with `code` and, in the same transaction,
    /// holds every queued row of its conversation, so the hold survives a restart.
    pub fn mark_interrupted(&mut self, id: i64, code: &str) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            &format!("UPDATE inbox SET state = 'needs_check', error = ?2, updated_at = {NOW} WHERE id = ?1"),
            params![id, code],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such inbox row"));
        }
        tx.execute(
            &hold_queued_sql("SELECT conversation_key FROM inbox WHERE id = ?1"),
            [id],
        )?;
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

    /// Startup recovery: `processing` becomes `needs_check` (never re-queued) and the
    /// queued rows of its conversation are held, both in one transaction;
    /// `reply_pending` rows stay as they are and are returned for re-sending.
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
        let mut needs_check = Vec::with_capacity(ids.len());
        for id in ids {
            needs_check.push(tx.query_row(
                &format!("SELECT {COLS} FROM inbox WHERE id = ?1"),
                [id],
                map_row,
            )?);
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
        })
    }

    /// `needs_check` rows whose notice has not been posted yet.
    pub fn needs_check_notices(&self) -> Result<Vec<InboxRow>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM inbox WHERE state = 'needs_check' AND notice_sent = 0 ORDER BY id"
        ))?;
        Ok(stmt.query_map([], map_row)?.collect::<Result<_, _>>()?)
    }

    pub fn set_omp_session_id(
        &self,
        conversation_key: &str,
        session_id: &str,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE conversations SET omp_session_id = ?2 WHERE conversation_key = ?1",
            params![conversation_key, session_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Invalid("no such conversation"));
        }
        Ok(())
    }

    pub fn omp_session_id(&self, conversation_key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT omp_session_id FROM conversations WHERE conversation_key = ?1",
                [conversation_key],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
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
        s.mark_interrupted(a, "E_TEST").unwrap();
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
        s.save_reply(id, "the answer").unwrap();
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
    fn omp_session_id_roundtrip() {
        let (_d, path, mut s) = open_tmp();
        admit(&mut s, "m1", "k1");
        assert_eq!(s.omp_session_id("k1").unwrap(), None);
        assert_eq!(s.omp_session_id("missing").unwrap(), None);
        s.set_omp_session_id("k1", "sess-1").unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.omp_session_id("k1").unwrap().as_deref(), Some("sess-1"));
        assert_eq!(
            s.set_omp_session_id("missing", "x").unwrap_err().code(),
            E_STORE_INVALID
        );
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

    #[test]
    fn database_file_is_owner_only() {
        let (_d, path, _s) = open_tmp();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }
}
