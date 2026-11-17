//! Memory history: commits and snapshots in the inbox database, import, dirty scan and accept.

use std::collections::{BTreeMap, BTreeSet};

use super::home::{MemoryError, MemoryHome};
use super::{
    BEGIN, E_MEMORY_ARGS, E_MEMORY_EXISTS, E_MEMORY_FILE, E_MEMORY_FRONTMATTER, E_MEMORY_HISTORY,
    E_MEMORY_NOOP, E_MEMORY_NOT_FOUND, E_MEMORY_PROTECTED, E_MEMORY_PROVENANCE, E_MEMORY_WRITE,
    END, MAX_FILE_BYTES, MAX_REASON_CHARS, PROTECTED, WRITER_HUMAN, WRITER_IMPORT,
};
use super::{frontmatter, guard};
use crate::store::{MemoryChange, MemoryCommit, Store, StoreError};

/// The reason of the one commit that imports the files found at the first start.
pub const IMPORT_REASON: &str = "import existing memory files";
/// The default reason of a `moraebot memory accept` commit.
pub const ACCEPT_REASON: &str = "accept hand edits";

/// The committed state: the latest non-deleted text of every recorded path.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Committed {
    pub files: BTreeMap<String, String>,
}

/// The result of an accept: the new commit, if any, and the paths it recorded.
#[derive(Debug, PartialEq)]
pub struct Accepted {
    pub commit: Option<i64>,
    pub paths: Vec<String>,
}

fn refused(code: &'static str, detail: impl Into<String>) -> MemoryError {
    MemoryError {
        code,
        detail: detail.into(),
    }
}

fn history(e: StoreError) -> MemoryError {
    refused(E_MEMORY_HISTORY, e.code())
}

/// The committed state read from the history tables.
pub fn committed(store: &Store) -> Result<Committed, MemoryError> {
    let files = store
        .memory_latest()
        .map_err(history)?
        .into_iter()
        .filter_map(|(path, text)| text.map(|text| (path, text)))
        .collect();
    Ok(Committed { files })
}

/// Brings the history and the working files in line before any use: finishes every commit a
/// crash left unapplied, records the one-time import on a database without commits, and
/// returns the committed state.
pub fn prepare(store: &mut Store, home: &MemoryHome) -> Result<Committed, MemoryError> {
    for (id, changes) in store.unapplied_memory_commits().map_err(history)? {
        materialize(home, &changes)?;
        store.mark_memory_applied(id).map_err(history)?;
    }
    if store.memory_commits().map_err(history)?.is_empty() {
        let changes: Vec<MemoryChange> = home
            .list(None)?
            .into_iter()
            .filter_map(|path| home.read(&path).ok().map(|text| (path, Some(text))))
            .collect();
        let commit = MemoryCommit {
            writer: WRITER_IMPORT.to_string(),
            reason: IMPORT_REASON.to_string(),
            turn_row: None,
            conversation_key: None,
            claim_json: None,
        };
        let id = store
            .record_memory_commit(&commit, &changes)
            .map_err(history)?;
        // The imported bytes are already the working files, so nothing is rewritten.
        store.mark_memory_applied(id).map_err(history)?;
        tracing::info!(
            commit = id,
            writer = WRITER_IMPORT,
            paths = changes.len(),
            "memory commit recorded"
        );
    }
    committed(store)
}

/// The sorted paths whose working file differs from the committed state: committed paths
/// that are changed, missing or unreadable, `.md` files that are not committed, and special
/// entries (symlinks, FIFOs and the like).
pub fn dirty(home: &MemoryHome, committed: &Committed) -> Result<Vec<String>, MemoryError> {
    let mut found = BTreeSet::new();
    for (path, text) in &committed.files {
        if home.read(path).ok().as_ref() != Some(text) {
            found.insert(path.clone());
        }
    }
    for path in home.list(None)? {
        if !committed.files.contains_key(&path) {
            found.insert(path);
        }
    }
    found.extend(home.special_entries());
    Ok(found.into_iter().collect())
}

/// The shared checks of a new file text: size, NUL and block sentinels, then frontmatter, then
/// an unchanged `provenance` against `previous` (an unparsable previous text has none), then
/// the secret table over the whole text.
pub fn check_text(text: &str, previous: Option<&str>) -> Result<(), MemoryError> {
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(refused(E_MEMORY_FILE, "too large"));
    }
    if text.contains('\0') {
        return Err(refused(E_MEMORY_FILE, "contains NUL"));
    }
    if text.contains(BEGIN) || text.contains(END) {
        return Err(refused(E_MEMORY_FILE, "contains a memory block sentinel"));
    }
    let parsed =
        frontmatter::parse(text).map_err(|e| refused(E_MEMORY_FRONTMATTER, e.to_string()))?;
    let before = previous
        .and_then(|previous| frontmatter::parse(previous).ok())
        .and_then(|old| old.frontmatter.get("provenance").map(str::to_owned));
    if parsed.frontmatter.get("provenance") != before.as_deref() {
        return Err(refused(E_MEMORY_PROVENANCE, "provenance cannot be changed"));
    }
    guard::refuse_secret("text", text)
}

/// A write reason: one non-blank line of at most `MAX_REASON_CHARS` characters that does not
/// look like a secret.
pub fn check_reason(reason: &str) -> Result<(), MemoryError> {
    if reason.trim().is_empty() {
        return Err(refused(E_MEMORY_ARGS, "reason is empty"));
    }
    if reason.contains(['\n', '\r']) {
        return Err(refused(E_MEMORY_ARGS, "reason must be one line"));
    }
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(refused(
            E_MEMORY_ARGS,
            format!("reason is longer than {MAX_REASON_CHARS} characters"),
        ));
    }
    guard::refuse_secret("reason", reason)
}

/// Records one commit, then writes its changes to the working files, then marks it applied.
/// When the files cannot be written the commit stays unapplied and the next `prepare`
/// finishes it.
pub fn record(
    store: &mut Store,
    home: &MemoryHome,
    commit: &MemoryCommit,
    changes: &[MemoryChange],
) -> Result<i64, MemoryError> {
    let id = store
        .record_memory_commit(commit, changes)
        .map_err(history)?;
    let mut ordered = changes.to_vec();
    ordered.sort();
    materialize(home, &ordered)?;
    store.mark_memory_applied(id).map_err(history)?;
    tracing::info!(
        commit = id,
        writer = %commit.writer,
        paths = changes.len(),
        "memory commit recorded"
    );
    Ok(id)
}

/// Records the owner's hand edits of `paths` (every dirty path when empty) as one commit by
/// the human writer. Every selected path must pass the text checks, and an untracked path on
/// disk must not look like a secret; the first refusal stops everything and nothing is
/// recorded. `read_only` does not apply here: the owner may change any file.
pub fn accept(
    store: &mut Store,
    home: &MemoryHome,
    paths: &[String],
    reason: &str,
) -> Result<Accepted, MemoryError> {
    check_reason(reason)?;
    let committed = prepare(store, home)?;
    let dirty = dirty(home, &committed)?;
    let selected: Vec<String> = if paths.is_empty() {
        dirty
    } else {
        let mut named = BTreeSet::new();
        for path in paths {
            home.resolve(path)?;
            if !dirty.contains(path) {
                return Err(refused(E_MEMORY_NOOP, format!("{path}: not changed")));
            }
            named.insert(path.clone());
        }
        named.into_iter().collect()
    };
    let mut changes: Vec<MemoryChange> = Vec::new();
    for path in &selected {
        if !committed.files.contains_key(path) {
            if std::fs::symlink_metadata(home.root().join(path)).is_ok() {
                guard::refuse_secret("path", path)?;
            }
            check_spelling(home, &committed, path)?;
        }
        let previous = committed.files.get(path).map(String::as_str);
        match home.read(path) {
            Ok(text) => {
                check_text(&text, previous)?;
                changes.push((path.clone(), Some(text)));
            }
            Err(e) if e.code == E_MEMORY_NOT_FOUND => {
                if PROTECTED.contains(&path.as_str()) {
                    return Err(refused(
                        E_MEMORY_PROTECTED,
                        format!("{path}: cannot be deleted"),
                    ));
                }
                changes.push((path.clone(), None));
            }
            Err(e) => return Err(refused(E_MEMORY_FILE, e.detail)),
        }
    }
    if changes.is_empty() {
        return Ok(Accepted {
            commit: None,
            paths: Vec::new(),
        });
    }
    let commit = MemoryCommit {
        writer: WRITER_HUMAN.to_string(),
        reason: reason.to_string(),
        turn_row: None,
        conversation_key: None,
        claim_json: None,
    };
    let id = record(store, home, &commit, &changes)?;
    Ok(Accepted {
        commit: Some(id),
        paths: selected,
    })
}

/// Refuses an untracked path that names a committed file or folder in another letter case:
/// the whole path against committed paths, every folder prefix against committed folder
/// prefixes, and the folders on disk by their exact names.
fn check_spelling(home: &MemoryHome, committed: &Committed, path: &str) -> Result<(), MemoryError> {
    let lower = path.to_lowercase();
    if let Some(existing) = committed.files.keys().find(|c| c.to_lowercase() == lower) {
        return Err(refused(
            E_MEMORY_EXISTS,
            format!("{path}: already committed as {existing}"),
        ));
    }
    let parts: Vec<&str> = path.split('/').collect();
    for committed_path in committed.files.keys() {
        let other: Vec<&str> = committed_path.split('/').collect();
        let folders = parts.len().min(other.len()) - 1;
        for depth in 1..=folders {
            let (mine, theirs) = (parts[..depth].join("/"), other[..depth].join("/"));
            if mine != theirs && mine.to_lowercase() == theirs.to_lowercase() {
                return Err(refused(
                    E_MEMORY_EXISTS,
                    format!("{path}: folder committed as {theirs}"),
                ));
            }
        }
    }
    home.exact_spelling(path)
}

/// Writes `changes` to the working files in the given order: `Some` atomically, `None` removed.
fn materialize(home: &MemoryHome, changes: &[MemoryChange]) -> Result<(), MemoryError> {
    for (path, text) in changes {
        let done = match text {
            Some(text) => home.write_atomic(path, text),
            None => home.remove(path),
        };
        done.map_err(|e| refused(E_MEMORY_WRITE, e.detail))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::Path;

    use super::*;
    use crate::memory::provenance::ClaimArgs;
    use crate::memory::{E_MEMORY_SECRET, HUMAN, PERSONA};

    const NOTE: &str = "---\ndescription: a note\n---\nv2-marker\n";

    fn fixture() -> (tempfile::TempDir, Store, MemoryHome) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("inbox.sqlite")).expect("store");
        let home = MemoryHome::new(dir.path());
        (dir, store, home)
    }

    fn laid_out() -> (tempfile::TempDir, Store, MemoryHome) {
        let (dir, store, home) = fixture();
        home.ensure_layout().expect("layout");
        (dir, store, home)
    }

    fn put(home: &MemoryHome, rel: &str, bytes: &[u8]) {
        let path = home.root().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, bytes).expect("write");
    }

    fn mkfifo(path: &Path) {
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("c path");
        // SAFETY: c_path is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo");
    }

    fn human_commit() -> MemoryCommit {
        MemoryCommit {
            writer: WRITER_HUMAN.to_string(),
            reason: "r".to_string(),
            turn_row: None,
            conversation_key: None,
            claim_json: None,
        }
    }

    fn count(store: &Store) -> usize {
        store.memory_commits().expect("commits").len()
    }

    /// Every entry under `root` with its bytes; a symlink maps to its target, a special file
    /// to nothing.
    fn bytes_under(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
                let path = entry.path();
                let kind = entry.file_type().expect("type");
                let rel = path.strip_prefix(base).expect("prefix");
                let rel = rel.to_string_lossy().into_owned();
                if kind.is_dir() {
                    walk(base, &path, out);
                } else if kind.is_symlink() {
                    let target = std::fs::read_link(&path).expect("link");
                    out.insert(rel, target.as_os_str().as_bytes().to_vec());
                } else if kind.is_file() {
                    out.insert(rel, std::fs::read(&path).expect("read"));
                } else {
                    out.insert(rel, Vec::new());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    /// Whether the existing folder `notes/x` is also reachable as `notes/X`.
    fn folds_case(home: &MemoryHome) -> bool {
        let probe = home.root().join("notes/x/probe");
        std::fs::write(&probe, b"p").expect("probe");
        let folds = home.root().join("notes/X/probe").exists();
        std::fs::remove_file(&probe).expect("remove probe");
        folds
    }

    fn mtimes(home: &MemoryHome, paths: &[&str]) -> Vec<std::time::SystemTime> {
        paths
            .iter()
            .map(|p| {
                std::fs::metadata(home.root().join(p))
                    .and_then(|meta| meta.modified())
                    .expect("mtime")
            })
            .collect()
    }

    #[test]
    fn first_prepare_imports_existing_files_once() {
        let (_dir, mut store, home) = laid_out();
        put(&home, "notes/n.md", NOTE.as_bytes());
        let paths = ["notes/n.md", HUMAN, PERSONA];
        let before = mtimes(&home, &paths);
        let committed = prepare(&mut store, &home).expect("prepare");
        let rows = store.memory_commits().expect("commits");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.commit.writer, WRITER_IMPORT);
        assert_eq!(row.commit.reason, IMPORT_REASON);
        assert!(row.applied);
        assert_eq!(row.commit.turn_row, None);
        assert_eq!(row.commit.conversation_key, None);
        assert_eq!(row.commit.claim_json, None);
        let snapshots = store.memory_snapshots(row.id).expect("snapshots");
        assert_eq!(snapshots.len(), 3);
        for (path, body) in &snapshots {
            let on_disk = std::fs::read_to_string(home.root().join(path)).expect("read");
            assert_eq!(body.as_deref(), Some(on_disk.as_str()), "{path}");
            assert_eq!(committed.files.get(path), Some(&on_disk), "{path}");
        }
        prepare(&mut store, &home).expect("second prepare");
        assert_eq!(count(&store), 1);
        assert_eq!(mtimes(&home, &paths), before);
    }

    #[test]
    fn empty_home_still_records_import() {
        let (_dir, mut store, home) = fixture();
        let committed = prepare(&mut store, &home).expect("prepare");
        let rows = store.memory_commits().expect("commits");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].applied);
        assert!(
            store
                .memory_snapshots(rows[0].id)
                .expect("snapshots")
                .is_empty()
        );
        assert!(committed.files.is_empty());
        put(&home, "notes/n.md", NOTE.as_bytes());
        let committed = prepare(&mut store, &home).expect("prepare again");
        assert_eq!(count(&store), 1);
        assert_eq!(dirty(&home, &committed).expect("dirty"), ["notes/n.md"]);
    }

    #[test]
    fn unreadable_file_is_not_imported_and_is_dirty() {
        let (_dir, mut store, home) = laid_out();
        put(
            &home,
            "notes/big.md",
            &vec![b'a'; MAX_FILE_BYTES as usize + 1],
        );
        let committed = prepare(&mut store, &home).expect("prepare");
        assert!(!committed.files.contains_key("notes/big.md"));
        assert_eq!(dirty(&home, &committed).expect("dirty"), ["notes/big.md"]);
    }

    #[test]
    fn unapplied_commit_is_materialized_by_prepare() {
        let (_dir, mut store, home) = laid_out();
        put(&home, "notes/gone.md", NOTE.as_bytes());
        let changes = vec![
            ("notes/gone.md".to_string(), None),
            ("notes/s.md".to_string(), Some(NOTE.to_string())),
        ];
        let id = store
            .record_memory_commit(&human_commit(), &changes)
            .expect("record");
        prepare(&mut store, &home).expect("prepare");
        let s = home.root().join("notes/s.md");
        assert_eq!(std::fs::read(&s).expect("s"), NOTE.as_bytes());
        assert_eq!(std::fs::metadata(&s).expect("meta").mode() & 0o777, 0o600);
        assert!(!home.root().join("notes/gone.md").exists());
        let rows = store.memory_commits().expect("commits");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert!(rows[0].applied);
    }

    #[test]
    fn dirty_lists_changed_missing_untracked_and_special() {
        let (_dir, mut store, home) = laid_out();
        for rel in ["notes/c.md", "notes/d.md", "notes/k.md"] {
            put(&home, rel, NOTE.as_bytes());
        }
        let committed = prepare(&mut store, &home).expect("prepare");
        assert!(dirty(&home, &committed).expect("clean").is_empty());
        put(&home, "notes/c.md", b"---\ndescription: changed\n---\n");
        std::fs::remove_file(home.root().join("notes/d.md")).expect("delete");
        put(&home, "notes/u.md", NOTE.as_bytes());
        symlink(
            home.root().join("notes/k.md"),
            home.root().join("notes/l.md"),
        )
        .expect("link");
        mkfifo(&home.root().join("notes/f.md"));
        assert_eq!(
            dirty(&home, &committed).expect("dirty"),
            [
                "notes/c.md",
                "notes/d.md",
                "notes/f.md",
                "notes/l.md",
                "notes/u.md"
            ]
        );
    }

    #[test]
    fn record_writes_files_after_the_row() {
        let (_dir, mut store, home) = laid_out();
        put(&home, "notes/old.md", NOTE.as_bytes());
        prepare(&mut store, &home).expect("prepare");
        let changes = vec![
            ("notes/old.md".to_string(), None),
            ("notes/new/a.md".to_string(), Some(NOTE.to_string())),
        ];
        let id = record(&mut store, &home, &human_commit(), &changes).expect("record");
        let row = store.memory_commits().expect("commits").pop().expect("row");
        assert_eq!(row.id, id);
        assert!(row.applied);
        let mut expected = changes.clone();
        expected.sort();
        assert_eq!(store.memory_snapshots(id).expect("snapshots"), expected);
        assert_eq!(
            std::fs::read(home.root().join("notes/new/a.md")).expect("new"),
            NOTE.as_bytes()
        );
        assert!(!home.root().join("notes/old.md").exists());
    }

    #[test]
    fn record_failure_leaves_commit_unapplied() {
        let (_dir, mut store, home) = laid_out();
        put(&home, "notes/a.md", NOTE.as_bytes());
        prepare(&mut store, &home).expect("prepare");
        let notes = home.root().join("notes");
        let new_text = "---\ndescription: a note\n---\nv3-marker\n";
        let changes = vec![("notes/a.md".to_string(), Some(new_text.to_string()))];
        std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        let result = record(&mut store, &home, &human_commit(), &changes);
        std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o700)).expect("restore");
        assert_eq!(result.expect_err("must fail").code, E_MEMORY_WRITE);
        let row = store.memory_commits().expect("commits").pop().expect("row");
        assert_eq!(row.commit.writer, WRITER_HUMAN);
        assert!(!row.applied);
        assert_eq!(
            std::fs::read(notes.join("a.md")).expect("old"),
            NOTE.as_bytes()
        );
        prepare(&mut store, &home).expect("prepare finishes it");
        assert!(
            store
                .memory_commits()
                .expect("commits")
                .iter()
                .all(|r| r.applied)
        );
        assert_eq!(
            std::fs::read(notes.join("a.md")).expect("new"),
            new_text.as_bytes()
        );
    }

    #[test]
    fn check_text_codes() {
        let big = format!(
            "---\ndescription: d\n---\n{}",
            "a".repeat(MAX_FILE_BYTES as usize)
        );
        let nul = "---\ndescription: d\n---\na\0b\n".to_string();
        let sentinel = format!("---\ndescription: d\n---\n{BEGIN}\n");
        for text in [&big, &nul, &sentinel] {
            assert_eq!(
                check_text(text, None).expect_err("file").code,
                E_MEMORY_FILE
            );
        }
        assert_eq!(
            check_text("description: d\nbody\n", None)
                .expect_err("fence")
                .code,
            E_MEMORY_FRONTMATTER
        );
        let plain = "---\ndescription: d\n---\nbody\n";
        let owner = "---\ndescription: d\nprovenance: owner\n---\nbody\n";
        let other = "---\ndescription: d\nprovenance: other\n---\nbody\n";
        for (new, previous) in [
            (owner, Some(plain)),
            (other, Some(owner)),
            (plain, Some(owner)),
            (owner, None),
        ] {
            assert_eq!(
                check_text(new, previous).expect_err("provenance").code,
                E_MEMORY_PROVENANCE
            );
        }
        check_text(owner, Some(owner)).expect("unchanged provenance");
        check_text(plain, Some("not frontmatter")).expect("unparsable previous");

        let pem = ["-----BEGIN ", "EC PRIVATE", " KEY-----"].concat();
        let secret = format!("---\ndescription: d\n---\n{pem}\n");
        let err = check_text(&secret, None).expect_err("secret");
        assert_eq!(err.code, E_MEMORY_SECRET);
        assert!(!err.detail.contains(&pem));
        assert_eq!(
            check_text(&format!("{pem}\n"), None)
                .expect_err("frontmatter first")
                .code,
            E_MEMORY_FRONTMATTER
        );
        let big_secret = format!("{secret}{}", "a".repeat(MAX_FILE_BYTES as usize));
        assert_eq!(
            check_text(&big_secret, None).expect_err("size first").code,
            E_MEMORY_FILE
        );
    }

    #[test]
    fn check_reason_codes() {
        let long = "a".repeat(MAX_REASON_CHARS + 1);
        for reason in ["", "   ", "a\nb", "a\rb", long.as_str()] {
            assert_eq!(check_reason(reason).expect_err(reason).code, E_MEMORY_ARGS);
        }
        check_reason(&"a".repeat(MAX_REASON_CHARS)).expect("200 characters");

        let key = fake(&["AK", "IA"], 'A', 16);
        let err = check_reason(&format!("note {key}")).expect_err("secret");
        assert_eq!(err.code, E_MEMORY_SECRET);
        assert!(!err.detail.contains(&key));
        let long_secret = format!("{key} {}", "a".repeat(MAX_REASON_CHARS));
        assert_eq!(
            check_reason(&long_secret).expect_err("length first").code,
            E_MEMORY_ARGS
        );
    }

    /// A secret-like value built at run time: `parts` joined, then `n` copies of `fill`, so no
    /// source line holds a whole token.
    fn fake(parts: &[&str], fill: char, n: usize) -> String {
        let mut out = parts.concat();
        out.extend(std::iter::repeat_n(fill, n));
        out
    }

    #[test]
    fn accept_refuses_secrets_and_records_nothing() {
        let (_dir, mut store, home) = laid_out();
        prepare(&mut store, &home).expect("prepare");
        let sk = fake(&["sk", "-"], 'a', 24);
        let persona = format!("---\ndescription: by hand\n---\nkey {sk}\n");
        put(&home, PERSONA, persona.as_bytes());
        for paths in [Vec::new(), strings(&[PERSONA])] {
            let err = accept(&mut store, &home, &paths, ACCEPT_REASON).expect_err("secret");
            assert_eq!(err.code, E_MEMORY_SECRET, "{paths:?}");
            assert!(!err.detail.contains(&sk));
            assert_eq!(count(&store), 1);
        }
        let gh_path = format!("notes/{}.md", fake(&["gh", "p_"], 'A', 36));
        put(&home, &gh_path, NOTE.as_bytes());
        let before = bytes_under(home.root());
        let err = accept(&mut store, &home, &strings(&[&gh_path]), ACCEPT_REASON)
            .expect_err("secret path");
        assert_eq!(err.code, E_MEMORY_SECRET);
        assert!(!err.detail.contains(&gh_path["notes/".len()..]));
        assert_eq!(count(&store), 1);
        assert_eq!(bytes_under(home.root()), before);
    }

    #[test]
    fn accept_removes_secret_like_path() {
        let (_dir, mut store, home) = laid_out();
        let sk_path = format!(
            "notes/{}.md",
            fake(&["sk", "-", "learn-pipeline-"], 'x', 12)
        );
        put(&home, &sk_path, NOTE.as_bytes());
        let imported = prepare(&mut store, &home).expect("import");
        assert!(imported.files.contains_key(&sk_path));
        std::fs::remove_file(home.root().join(&sk_path)).expect("delete by hand");
        let accepted = accept(&mut store, &home, &[], ACCEPT_REASON).expect("accept");
        assert_eq!(accepted.paths, strings(&[&sk_path]));
        let row = store.memory_commits().expect("commits").pop().expect("row");
        assert_eq!(Some(row.id), accepted.commit);
        assert_eq!(row.commit.writer, WRITER_HUMAN);
        assert_eq!(
            store.memory_snapshots(row.id).expect("snapshots"),
            vec![(sk_path.clone(), None)]
        );
        let committed = committed(&store).expect("committed");
        assert!(dirty(&home, &committed).expect("dirty").is_empty());

        assert_eq!(
            store
                .admit("test", "m1", "k1", "chan1", "111", "hi")
                .expect("admit"),
            crate::store::Admitted::New(1)
        );
        let store = std::sync::Mutex::new(store);
        let turn = crate::omp::TurnContext {
            row_id: 1,
            conversation_key: "k1".to_string(),
        };
        let edit = crate::memory::write::Edit::Create {
            path: "notes/c.md".to_string(),
            description: "c".to_string(),
            body: "c".to_string(),
        };
        crate::memory::write::apply(
            &store,
            &home,
            Some(&turn),
            edit,
            "after accept",
            &ClaimArgs::default(),
        )
        .expect("lead write after accept");
    }

    #[test]
    fn accept_changes_read_only_files() {
        let (_dir, mut store, home) = laid_out();
        put(
            &home,
            "notes/ro.md",
            b"---\ndescription: locked\nread_only: \"true\"\n---\nbody\n",
        );
        prepare(&mut store, &home).expect("prepare");
        let edited = "---\ndescription: locked\nread_only: \"true\"\n---\nby hand\n";
        assert!(edited.contains("read_only: \"true\""));
        put(&home, "notes/ro.md", edited.as_bytes());
        let accepted = accept(&mut store, &home, &[], ACCEPT_REASON).expect("accept");
        let id = accepted.commit.expect("commit");
        let row = store.memory_commits().expect("commits").pop().expect("row");
        assert_eq!(row.id, id);
        assert_eq!(row.commit.writer, WRITER_HUMAN);
        assert_eq!(
            store.memory_snapshots(id).expect("snapshots"),
            vec![("notes/ro.md".to_string(), Some(edited.to_string()))]
        );
    }

    #[test]
    fn import_keeps_files_as_found() {
        let (_dir, mut store, home) = laid_out();
        let text = format!(
            "---\ndescription: s\n---\n{}\n",
            fake(&["pass", "word="], 'q', 12)
        );
        assert!(guard::find_secret(&text).is_some());
        put(&home, "notes/s.md", text.as_bytes());
        let committed = prepare(&mut store, &home).expect("import");
        assert_eq!(committed.files.get("notes/s.md"), Some(&text));
        let row = store.memory_commits().expect("commits").pop().expect("row");
        assert_eq!(row.commit.writer, WRITER_IMPORT);
        assert!(
            store
                .memory_snapshots(row.id)
                .expect("snapshots")
                .contains(&("notes/s.md".to_string(), Some(text.clone())))
        );
        assert!(dirty(&home, &committed).expect("dirty").is_empty());
    }

    #[test]
    fn accept_commits_hand_edits_as_human() {
        let (_dir, mut store, home) = laid_out();
        prepare(&mut store, &home).expect("prepare");
        let persona = "---\ndescription: edited by hand\n---\nv2-marker\n";
        put(&home, PERSONA, persona.as_bytes());
        put(&home, "notes/u.md", NOTE.as_bytes());
        let accepted = accept(&mut store, &home, &[], ACCEPT_REASON).expect("accept");
        assert_eq!(accepted.paths, strings(&["notes/u.md", PERSONA]));
        let id = accepted.commit.expect("commit");
        let rows = store.memory_commits().expect("commits");
        assert_eq!(rows.len(), 2);
        let row = rows.last().expect("row");
        assert_eq!(row.id, id);
        assert_eq!(row.commit.writer, WRITER_HUMAN);
        assert_eq!(row.commit.reason, ACCEPT_REASON);
        assert!(row.applied);
        assert_eq!(
            store.memory_snapshots(id).expect("snapshots"),
            vec![
                ("notes/u.md".to_string(), Some(NOTE.to_string())),
                (PERSONA.to_string(), Some(persona.to_string())),
            ]
        );
        let committed = committed(&store).expect("committed");
        assert!(dirty(&home, &committed).expect("dirty").is_empty());
        let none = accept(&mut store, &home, &[], ACCEPT_REASON).expect("nothing left");
        assert_eq!(
            none,
            Accepted {
                commit: None,
                paths: Vec::new()
            }
        );
        assert_eq!(count(&store), 2);
    }

    #[test]
    fn accept_named_clean_path_is_noop() {
        let (_dir, mut store, home) = laid_out();
        prepare(&mut store, &home).expect("prepare");
        put(&home, "notes/u.md", NOTE.as_bytes());
        let err =
            accept(&mut store, &home, &strings(&[PERSONA]), ACCEPT_REASON).expect_err("clean path");
        assert_eq!(err.code, E_MEMORY_NOOP);
        assert_eq!(count(&store), 1);
    }

    #[test]
    fn accept_refusals_record_nothing() {
        type Setup = fn(&MemoryHome);
        let cases: [(Setup, &str); 4] = [
            (
                |home| put(home, "notes/bad.md", b"no fence\n"),
                E_MEMORY_FRONTMATTER,
            ),
            (
                |home| {
                    symlink(home.root().join(HUMAN), home.root().join("system/l.md")).expect("link")
                },
                E_MEMORY_FILE,
            ),
            (
                |home| std::fs::remove_file(home.root().join(PERSONA)).expect("delete"),
                E_MEMORY_PROTECTED,
            ),
            (
                |home| {
                    put(
                        home,
                        HUMAN,
                        b"---\ndescription: owner\nprovenance: x\n---\nbody\n",
                    )
                },
                E_MEMORY_PROVENANCE,
            ),
        ];
        for (setup, code) in cases {
            let (_dir, mut store, home) = laid_out();
            prepare(&mut store, &home).expect("prepare");
            put(&home, "notes/good.md", NOTE.as_bytes());
            setup(&home);
            let before = bytes_under(home.root());
            let err = accept(&mut store, &home, &[], ACCEPT_REASON).expect_err(code);
            assert_eq!(err.code, code);
            assert_eq!(count(&store), 1, "{code}");
            assert_eq!(bytes_under(home.root()), before, "{code}");
        }
    }

    #[test]
    fn accept_case_fold_onto_committed_is_refused() {
        let (_dir, mut store, home) = laid_out();
        put(&home, "notes/x/keep.md", NOTE.as_bytes());
        put(&home, "notes/a.md", NOTE.as_bytes());
        prepare(&mut store, &home).expect("prepare");
        if folds_case(&home) {
            std::fs::rename(
                home.root().join("notes/a.md"),
                home.root().join("notes/A.md"),
            )
            .expect("rename by hand");
        } else {
            put(&home, "notes/A.md", NOTE.as_bytes());
        }
        let before = bytes_under(home.root());
        for paths in [Vec::new(), strings(&["notes/A.md"])] {
            let err = accept(&mut store, &home, &paths, ACCEPT_REASON).expect_err("fold");
            assert_eq!(err.code, E_MEMORY_EXISTS, "{paths:?}");
            assert_eq!(count(&store), 1);
            assert_eq!(bytes_under(home.root()), before);
        }
    }

    #[test]
    fn accept_folder_case_fold_is_refused() {
        let (_dir, mut store, home) = laid_out();
        put(&home, "notes/x/1.md", NOTE.as_bytes());
        prepare(&mut store, &home).expect("prepare");
        let folds = folds_case(&home);
        put(&home, "notes/X/2.md", NOTE.as_bytes());
        if folds {
            assert_eq!(
                home.list(Some("notes")).expect("list"),
                ["notes/x/1.md", "notes/x/2.md"]
            );
            let accepted = accept(
                &mut store,
                &home,
                &strings(&["notes/x/2.md"]),
                ACCEPT_REASON,
            )
            .expect("same spelling");
            assert_eq!(accepted.paths, strings(&["notes/x/2.md"]));
            assert!(accepted.commit.is_some());
            assert_eq!(count(&store), 2);
        } else {
            let before = bytes_under(home.root());
            for paths in [strings(&["notes/X/2.md"]), Vec::new()] {
                let err = accept(&mut store, &home, &paths, ACCEPT_REASON).expect_err("fold");
                assert_eq!(err.code, E_MEMORY_EXISTS, "{paths:?}");
                assert_eq!(count(&store), 1);
                assert_eq!(bytes_under(home.root()), before);
            }
        }
    }

    #[test]
    fn accept_records_a_deletion_as_null() {
        let (_dir, mut store, home) = laid_out();
        put(&home, "notes/n.md", NOTE.as_bytes());
        prepare(&mut store, &home).expect("prepare");
        std::fs::remove_file(home.root().join("notes/n.md")).expect("delete by hand");
        let accepted = accept(&mut store, &home, &[], ACCEPT_REASON).expect("accept");
        let id = accepted.commit.expect("commit");
        assert_eq!(
            store.memory_snapshots(id).expect("snapshots"),
            vec![("notes/n.md".to_string(), None)]
        );
        let committed = committed(&store).expect("committed");
        assert!(!committed.files.contains_key("notes/n.md"));
        assert!(dirty(&home, &committed).expect("dirty").is_empty());
    }
}
