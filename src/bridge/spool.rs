//! The bridge's inbound spool (issue #40, `docs/bridge.md` "Inbound messages and the spool").
//!
//! A bridge writes each inbound messenger payload here before it acks its messenger, and
//! deletes it when the core acks it, so neither a core outage nor a bridge restart loses a
//! message. The spool is messenger-neutral: it stores the raw payload as JSON and knows
//! nothing about its shape.
//!
//! Layout: one file per message, `<dir>/<seq, 20 digits>.json`, written to a temp file,
//! synced and renamed into place. The folder has mode 0700 and the files 0600, because
//! they hold message text and file URLs. Each file keeps the receive time and the number
//! of connections the message was sent on, so the age limit and the attempt limit hold
//! across bridge restarts.
//!
//! The methods do blocking file I/O on small files under one lock; async callers may run
//! them in place or through `spawn_blocking`.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::E_BRIDGE_SPOOL;
use super::peer::own_uid;

/// Most messages the spool holds; a message arriving at this count is dropped.
pub const MAX_ENTRIES: usize = 10_000;
/// Messages older than this (seconds) are dropped at open and by [`Spool::drop_expired`].
pub const MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;
/// Connections a message is sent on before the bridge gives up on it.
pub const MAX_ATTEMPTS: u32 = 5;

const SEQ_DIGITS: usize = 20;
const SUFFIX: &str = ".json";
const TMP_SUFFIX: &str = ".tmp";

/// A spool operation failed on the file system.
#[derive(Debug)]
pub struct SpoolError {
    pub context: String,
    pub source: io::Error,
}

impl SpoolError {
    fn new(context: impl Into<String>, source: io::Error) -> Self {
        Self {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for SpoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{E_BRIDGE_SPOOL}: {}: {}", self.context, self.source)
    }
}

impl std::error::Error for SpoolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// One spooled message.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Position in the spool; later messages have larger numbers.
    pub seq: u64,
    /// Unix seconds when the bridge received it.
    pub received_at: i64,
    /// Connections it was sent on, the current one included.
    pub attempts: u32,
    /// The raw messenger payload.
    pub payload: Value,
}

/// The outcome of [`Spool::start_attempt`].
#[derive(Debug, Clone, PartialEq)]
pub enum Attempt {
    /// Send this entry; `redelivery` is true when an earlier connection already got it.
    Send { entry: Entry, redelivery: bool },
    /// The entry used up [`MAX_ATTEMPTS`] and was removed (logged).
    Exhausted,
    /// The entry is no longer in the spool: acked, expired, or unreadable and removed.
    Gone,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    received_at: i64,
    attempts: u32,
    payload: Value,
}

struct State {
    next_seq: u64,
    len: usize,
}

/// The spool folder of one bridge. Share it between tasks behind an `Arc`.
pub struct Spool {
    dir: PathBuf,
    limit: usize,
    state: Mutex<State>,
}

impl Spool {
    /// Opens the spool in `dir`, creating the folder with mode 0700 when missing. Removes
    /// temp files a crash left behind and drops messages older than [`MAX_AGE_SECS`]
    /// at `now` (unix seconds).
    pub fn open(dir: &Path, now: i64) -> Result<Self, SpoolError> {
        prepare_dir(dir)?;
        let mut seqs = Vec::new();
        for name in list(dir)? {
            if let Some(seq) = parse_name(&name) {
                seqs.push(seq);
            } else if name.starts_with('.') && name.ends_with(TMP_SUFFIX) {
                let path = dir.join(&name);
                fs::remove_file(&path)
                    .map_err(|err| SpoolError::new(format!("remove {}", path.display()), err))?;
            } else {
                tracing::warn!(file = %name, "unexpected file in the spool folder, left alone");
            }
        }
        let spool = Self {
            dir: dir.to_path_buf(),
            limit: MAX_ENTRIES,
            state: Mutex::new(State {
                next_seq: seqs.iter().max().map_or(1, |max| max + 1),
                len: seqs.len(),
            }),
        };
        spool.drop_expired(now)?;
        Ok(spool)
    }

    /// Messages in the spool.
    pub fn len(&self) -> usize {
        self.lock().len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Writes `payload`, received at `now`, durably to the spool and returns its seq. At
    /// the size limit the message is dropped with an error log and `Ok(None)` comes back.
    /// On `Err` nothing was stored, so the caller must not ack its messenger.
    pub fn append(&self, payload: &Value, now: i64) -> Result<Option<u64>, SpoolError> {
        let mut state = self.lock();
        if state.len >= self.limit {
            tracing::error!(
                code = E_BRIDGE_SPOOL,
                limit = self.limit,
                "the spool is full, inbound message dropped"
            );
            return Ok(None);
        }
        let seq = state.next_seq;
        state.next_seq += 1;
        self.write(
            seq,
            &Stored {
                received_at: now,
                attempts: 0,
                payload: payload.clone(),
            },
        )?;
        state.len += 1;
        Ok(Some(seq))
    }

    /// The seqs of all spooled messages, oldest first.
    pub fn pending(&self) -> Result<Vec<u64>, SpoolError> {
        let _state = self.lock();
        self.seqs()
    }

    /// Counts one more connection for message `seq` and returns it for sending, or
    /// removes it when it already used [`MAX_ATTEMPTS`].
    pub fn start_attempt(&self, seq: u64) -> Result<Attempt, SpoolError> {
        let mut state = self.lock();
        let Some(mut stored) = self.read(&mut state, seq)? else {
            return Ok(Attempt::Gone);
        };
        if stored.attempts >= MAX_ATTEMPTS {
            self.unlink(&mut state, seq)?;
            tracing::error!(
                code = E_BRIDGE_SPOOL,
                seq,
                attempts = stored.attempts,
                "spooled message dropped after its last delivery attempt"
            );
            return Ok(Attempt::Exhausted);
        }
        let redelivery = stored.attempts > 0;
        stored.attempts += 1;
        self.write(seq, &stored)?;
        Ok(Attempt::Send {
            entry: Entry {
                seq,
                received_at: stored.received_at,
                attempts: stored.attempts,
                payload: stored.payload,
            },
            redelivery,
        })
    }

    /// Deletes message `seq` (on the core's ack, or when it is not for the core). True
    /// when it was still there.
    pub fn remove(&self, seq: u64) -> Result<bool, SpoolError> {
        let mut state = self.lock();
        self.unlink(&mut state, seq)
    }

    /// Drops every message received more than [`MAX_AGE_SECS`] before `now`, each with a
    /// warning, and returns how many went.
    pub fn drop_expired(&self, now: i64) -> Result<usize, SpoolError> {
        let mut state = self.lock();
        let mut dropped = 0;
        for seq in self.seqs()? {
            let Some(stored) = self.read(&mut state, seq)? else {
                continue;
            };
            if now - stored.received_at > MAX_AGE_SECS {
                self.unlink(&mut state, seq)?;
                tracing::warn!(
                    seq,
                    received_at = stored.received_at,
                    "spooled message older than 7 days dropped"
                );
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is two counters updated after each file operation, so a panic in
        // another holder leaves it usable.
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq:0SEQ_DIGITS$}{SUFFIX}"))
    }

    fn seqs(&self) -> Result<Vec<u64>, SpoolError> {
        let mut seqs: Vec<u64> = list(&self.dir)?
            .iter()
            .filter_map(|name| parse_name(name))
            .collect();
        seqs.sort_unstable();
        Ok(seqs)
    }

    /// Reads entry `seq`. `None` when it is missing, or unreadable (then it is removed
    /// with an error log, so one damaged file cannot block the spool).
    fn read(&self, state: &mut State, seq: u64) -> Result<Option<Stored>, SpoolError> {
        let path = self.path(seq);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(SpoolError::new(format!("read {}", path.display()), err)),
        };
        match serde_json::from_slice(&bytes) {
            Ok(stored) => Ok(Some(stored)),
            Err(err) => {
                self.unlink(state, seq)?;
                tracing::error!(
                    code = E_BRIDGE_SPOOL,
                    seq,
                    error = %err,
                    "unreadable spooled message removed"
                );
                Ok(None)
            }
        }
    }

    /// Writes entry `seq` through a temp file: created new with mode 0600, synced, renamed
    /// into place, then the folder synced so the rename survives a power loss.
    fn write(&self, seq: u64, stored: &Stored) -> Result<(), SpoolError> {
        let path = self.path(seq);
        let tmp = self
            .dir
            .join(format!(".{seq:0SEQ_DIGITS$}{SUFFIX}{TMP_SUFFIX}"));
        let result = write_synced(&tmp, stored).and_then(|()| fs::rename(&tmp, &path));
        if let Err(err) = result {
            let _ = fs::remove_file(&tmp);
            return Err(SpoolError::new(format!("write {}", path.display()), err));
        }
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|err| SpoolError::new(format!("sync {}", self.dir.display()), err))
    }

    fn unlink(&self, state: &mut State, seq: u64) -> Result<bool, SpoolError> {
        let path = self.path(seq);
        match fs::remove_file(&path) {
            Ok(()) => {
                state.len = state.len.saturating_sub(1);
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(SpoolError::new(format!("remove {}", path.display()), err)),
        }
    }

    #[cfg(test)]
    fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }
}

fn write_synced(tmp: &Path, stored: &Stored) -> io::Result<()> {
    let bytes = serde_json::to_vec(stored).map_err(io::Error::other)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

/// The seq of a spool file name (`<20 digits>.json`), `None` for any other name.
fn parse_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(SUFFIX)?;
    if digits.len() != SEQ_DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn list(dir: &Path) -> Result<Vec<String>, SpoolError> {
    let context = || format!("list {}", dir.display());
    let mut names = Vec::new();
    for item in fs::read_dir(dir).map_err(|err| SpoolError::new(context(), err))? {
        let item = item.map_err(|err| SpoolError::new(context(), err))?;
        if let Ok(name) = item.file_name().into_string() {
            names.push(name);
        }
    }
    Ok(names)
}

/// Creates `dir` with mode 0700 when missing, tightens it to 0700 when it is ours but
/// open to others, and refuses a symlink, a non-folder, or a folder of another uid.
fn prepare_dir(dir: &Path) -> Result<(), SpoolError> {
    let refuse = |why: &str| {
        SpoolError::new(
            format!("spool folder {}", dir.display()),
            io::Error::new(io::ErrorKind::PermissionDenied, why.to_string()),
        )
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|err| SpoolError::new(format!("create {}", dir.display()), err))?;
    let meta = fs::symlink_metadata(dir)
        .map_err(|err| SpoolError::new(format!("inspect {}", dir.display()), err))?;
    if !meta.file_type().is_dir() {
        return Err(refuse("not a folder"));
    }
    if meta.uid() != own_uid() {
        return Err(refuse("owned by another uid"));
    }
    if meta.mode() & 0o777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|err| SpoolError::new(format!("chmod 0700 {}", dir.display()), err))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_700_000_000;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names = list(dir).unwrap();
        names.sort();
        names
    }

    fn send(spool: &Spool, seq: u64) -> (Entry, bool) {
        match spool.start_attempt(seq).unwrap() {
            Attempt::Send { entry, redelivery } => (entry, redelivery),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn an_append_is_a_private_file_named_by_seq() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("spool");
        let spool = Spool::open(&dir, NOW).unwrap();
        assert_eq!(mode(&dir), 0o700);

        let seq = spool.append(&json!({"text": "hi"}), NOW).unwrap();
        assert_eq!(seq, Some(1));
        assert_eq!(files(&dir), ["00000000000000000001.json"]);
        assert_eq!(mode(&dir.join("00000000000000000001.json")), 0o600);
        assert_eq!(spool.len(), 1);

        let (entry, redelivery) = send(&spool, 1);
        assert!(!redelivery);
        assert_eq!(
            entry,
            Entry {
                seq: 1,
                received_at: NOW,
                attempts: 1,
                payload: json!({"text": "hi"}),
            }
        );
    }

    #[test]
    fn pending_is_in_order_and_remove_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::open(tmp.path(), NOW).unwrap();
        for n in 0..3 {
            spool.append(&json!(n), NOW).unwrap();
        }
        assert_eq!(spool.pending().unwrap(), [1, 2, 3]);

        assert!(spool.remove(2).unwrap());
        assert!(!spool.remove(2).unwrap(), "a second ack finds nothing");
        assert_eq!(spool.pending().unwrap(), [1, 3]);
        assert_eq!(spool.len(), 2);
        assert_eq!(spool.start_attempt(2).unwrap(), Attempt::Gone);
    }

    #[test]
    fn the_spool_survives_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let spool = Spool::open(tmp.path(), NOW).unwrap();
            spool.append(&json!("a"), NOW).unwrap();
            spool.append(&json!("b"), NOW).unwrap();
            spool.append(&json!("c"), NOW).unwrap();
            spool.remove(1).unwrap();
            send(&spool, 2);
        }
        let spool = Spool::open(tmp.path(), NOW + 60).unwrap();
        assert_eq!(spool.len(), 2);
        assert_eq!(spool.pending().unwrap(), [2, 3]);
        let (entry, redelivery) = send(&spool, 2);
        assert!(redelivery, "sent before the restart");
        assert_eq!(entry.payload, json!("b"));
        assert_eq!(entry.attempts, 2);
        assert_eq!(spool.append(&json!("d"), NOW).unwrap(), Some(4));
    }

    #[test]
    fn a_message_is_dropped_after_its_last_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::open(tmp.path(), NOW).unwrap();
        spool.append(&json!("poison"), NOW).unwrap();
        for attempt in 1..=MAX_ATTEMPTS {
            let (entry, redelivery) = send(&spool, 1);
            assert_eq!(entry.attempts, attempt);
            assert_eq!(redelivery, attempt > 1);
        }
        assert_eq!(spool.start_attempt(1).unwrap(), Attempt::Exhausted);
        assert!(spool.is_empty());
        assert!(files(tmp.path()).is_empty());
    }

    #[test]
    fn old_messages_are_dropped_at_open_and_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let spool = Spool::open(tmp.path(), NOW).unwrap();
            spool.append(&json!("old"), NOW - MAX_AGE_SECS - 1).unwrap();
            spool.append(&json!("edge"), NOW - MAX_AGE_SECS).unwrap();
            spool.append(&json!("new"), NOW).unwrap();
        }
        let spool = Spool::open(tmp.path(), NOW).unwrap();
        assert_eq!(spool.pending().unwrap(), [2, 3]);
        assert_eq!(spool.len(), 2);

        assert_eq!(spool.drop_expired(NOW + 1).unwrap(), 1);
        assert_eq!(spool.pending().unwrap(), [3]);
        assert_eq!(spool.drop_expired(NOW + 1).unwrap(), 0);
    }

    #[test]
    fn a_full_spool_drops_new_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spool = Spool::open(tmp.path(), NOW).unwrap();
        spool.set_limit(2);
        assert_eq!(spool.append(&json!(1), NOW).unwrap(), Some(1));
        assert_eq!(spool.append(&json!(2), NOW).unwrap(), Some(2));
        assert_eq!(spool.append(&json!(3), NOW).unwrap(), None);
        assert_eq!(spool.pending().unwrap(), [1, 2]);

        spool.remove(1).unwrap();
        assert_eq!(spool.append(&json!(4), NOW).unwrap(), Some(3));
    }

    #[test]
    fn a_failed_append_stores_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::open(tmp.path(), NOW).unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = spool.append(&json!("lost"), NOW);
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();

        let err = result.expect_err("a read-only folder fails the append");
        assert!(err.to_string().starts_with(E_BRIDGE_SPOOL));
        assert!(spool.is_empty());
        assert!(files(tmp.path()).is_empty());
    }

    #[test]
    fn open_cleans_temp_files_and_skips_strangers() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".00000000000000000007.json.tmp"), b"{").unwrap();
        fs::write(tmp.path().join("notes.txt"), b"x").unwrap();
        fs::write(tmp.path().join("00000000000000000005.json"), b"{broken").unwrap();

        let spool = Spool::open(tmp.path(), NOW).unwrap();
        assert_eq!(
            files(tmp.path()),
            ["notes.txt"],
            "the temp file and the damaged entry are removed at open"
        );
        assert!(spool.is_empty());
        assert_eq!(spool.start_attempt(5).unwrap(), Attempt::Gone);
        assert_eq!(spool.append(&json!("x"), NOW).unwrap(), Some(6));
        assert_eq!(spool.pending().unwrap(), [6]);
        assert_eq!(spool.len(), 1);
    }

    #[test]
    fn an_open_folder_is_tightened_and_a_symlink_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("spool");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        Spool::open(&dir, NOW).unwrap();
        assert_eq!(mode(&dir), 0o700);

        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = Spool::open(&link, NOW).err().expect("symlink refused");
        assert!(err.to_string().starts_with(E_BRIDGE_SPOOL));
    }
}
