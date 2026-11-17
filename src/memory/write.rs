//! The memory write engine: validation, the origin gate, recording and applying changes.

use std::sync::Mutex;

use super::frontmatter::{self, Frontmatter, render_frontmatter};
use super::guard;
use super::history::{self, Committed};
use super::home::{MemoryError, MemoryHome};
use super::provenance::{self, Claim, ClaimArgs, Kind};
use super::{
    AGREEMENTS_FILE, BEGIN, BOUNDARIES, E_MEMORY_AGREEMENT, E_MEMORY_ARGS, E_MEMORY_DIRTY,
    E_MEMORY_EXISTS, E_MEMORY_FILE, E_MEMORY_FRONTMATTER, E_MEMORY_HISTORY, E_MEMORY_NO_MATCH,
    E_MEMORY_NOOP, E_MEMORY_NOT_FOUND, E_MEMORY_ORIGIN, E_MEMORY_PATH, E_MEMORY_PROTECTED,
    E_MEMORY_QUOTE, E_MEMORY_READ_ONLY, END, MAX_DIRTY_LISTED, MAX_FILE_BYTES, MEMORY_CREATE_TOOL,
    MEMORY_DELETE_TOOL, MEMORY_INSERT_TOOL, MEMORY_RENAME_TOOL, MEMORY_STR_REPLACE_TOOL,
    MEMORY_UPDATE_DESCRIPTION_TOOL, PROTECTED, SOUL_PATHS, WRITER_LEAD,
};
use crate::omp::{TurnContext, lock};
use crate::store::{MemoryChange, MemoryCommit, Store};

/// One write the lead asked for through a memory tool.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    Create {
        path: String,
        description: String,
        body: String,
    },
    StrReplace {
        path: String,
        old_string: String,
        new_string: String,
    },
    Insert {
        path: String,
        insert_line: usize,
        insert_text: String,
    },
    Delete {
        path: String,
    },
    Rename {
        old_path: String,
        new_path: String,
    },
    UpdateDescription {
        path: String,
        description: String,
    },
}

impl Edit {
    /// The memory tool that makes this edit.
    pub fn tool(&self) -> &'static str {
        match self {
            Self::Create { .. } => MEMORY_CREATE_TOOL,
            Self::StrReplace { .. } => MEMORY_STR_REPLACE_TOOL,
            Self::Insert { .. } => MEMORY_INSERT_TOOL,
            Self::Delete { .. } => MEMORY_DELETE_TOOL,
            Self::Rename { .. } => MEMORY_RENAME_TOOL,
            Self::UpdateDescription { .. } => MEMORY_UPDATE_DESCRIPTION_TOOL,
        }
    }

    /// The path the edit works on (the source of a rename).
    fn path(&self) -> &str {
        match self {
            Self::Create { path, .. }
            | Self::StrReplace { path, .. }
            | Self::Insert { path, .. }
            | Self::Delete { path }
            | Self::UpdateDescription { path, .. } => path,
            Self::Rename { old_path, .. } => old_path,
        }
    }
}

/// A recorded write: its commit, the tool, the path, for a rename the new path, the tool
/// arguments that were split back out of a mis-closed parameter, the checked claim when
/// the write named a `source`, and whether the commit changed the persona or identity.
#[derive(Debug, PartialEq)]
pub struct Applied {
    pub commit: i64,
    pub tool: &'static str,
    pub path: String,
    pub to: Option<String>,
    pub repaired: Vec<String>,
    pub claim: Option<Claim>,
    pub soul_edit: bool,
}

impl Applied {
    /// The tool result: `{"committed", "command", "path"}`, plus `"to"` for a rename,
    /// `"repaired"` when arguments were repaired, and `"source"`, `"downgraded"` and (when set)
    /// `"code"` when the write carried a claim, and `"soul_edit"` (only when true) when the
    /// commit changed a soul file.
    pub fn to_json(&self) -> String {
        let mut value = serde_json::json!({
            "committed": self.commit,
            "command": self.tool,
            "path": self.path,
        });
        if let Some(to) = &self.to {
            value["to"] = serde_json::json!(to);
        }
        if !self.repaired.is_empty() {
            value["repaired"] = serde_json::json!(self.repaired);
        }
        if let Some(claim) = &self.claim {
            value["source"] = serde_json::json!(claim.kind.name());
            value["downgraded"] = serde_json::json!(claim.downgraded());
            if let Some(code) = claim.code {
                value["code"] = serde_json::json!(code);
            }
        }
        if self.soul_edit {
            value["soul_edit"] = serde_json::json!(true);
        }
        value.to_string()
    }
}

fn refused(code: &'static str, detail: impl Into<String>) -> MemoryError {
    MemoryError {
        code,
        detail: detail.into(),
    }
}

/// Checks `edit` against the running turn, the paths, the history and the new text, then
/// records it as one lead commit and applies it to the working files. The store stays locked
/// from the origin check to the end, so writers never interleave. A path that looks like a
/// secret is logged as `-`. `claim` is checked and recorded only when it names a `source`.
pub fn apply(
    store: &Mutex<Store>,
    home: &MemoryHome,
    turn: Option<&TurnContext>,
    edit: Edit,
    reason: &str,
    claim: &ClaimArgs,
) -> Result<Applied, MemoryError> {
    let tool = edit.tool();
    let path = match guard::find_secret(edit.path()) {
        Some(_) => "-".to_string(),
        None => edit.path().to_string(),
    };
    let result = run(store, home, turn, edit, reason, claim);
    match &result {
        Ok(applied) => {
            tracing::info!(tool, path = %path, commit = applied.commit, "memory write committed");
        }
        Err(e) => tracing::info!(tool, path = %path, code = e.code, "memory write refused"),
    }
    result
}

fn run(
    store: &Mutex<Store>,
    home: &MemoryHome,
    turn: Option<&TurnContext>,
    edit: Edit,
    reason: &str,
    claim: &ClaimArgs,
) -> Result<Applied, MemoryError> {
    history::check_reason(reason)?;
    if let Some(quote) = &claim.user_quote {
        guard::refuse_secret("user_quote", quote)?;
    }

    let mut store = lock(store);
    let turn = owner_turn(&store, turn)?;

    check_paths(home, &edit)?;

    let committed = history::prepare(&mut store, home)?;
    let dirty = history::dirty(home, &committed)?;
    if !dirty.is_empty() {
        let mut detail = dirty
            .iter()
            .take(MAX_DIRTY_LISTED)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if dirty.len() > MAX_DIRTY_LISTED {
            detail.push_str(&format!(" and {} more", dirty.len() - MAX_DIRTY_LISTED));
        }
        return Err(refused(E_MEMORY_DIRTY, detail));
    }

    check_existence(home, &committed, &edit)?;
    check_read_only(&committed, &edit)?;

    let (mut changes, previous): (Vec<MemoryChange>, Vec<Option<String>>) =
        new_texts(&committed, &edit)?.into_iter().unzip();
    for ((_, text), previous) in changes.iter().zip(&previous) {
        if let Some(text) = text {
            history::check_text(text, previous.as_deref())?;
        }
    }
    let noop = changes.iter().all(|(path, text)| match text {
        Some(text) => committed.files.get(path) == Some(text),
        None => !committed.files.contains_key(path),
    });
    if noop {
        return Err(refused(E_MEMORY_NOOP, "nothing would change"));
    }

    let claim = match claim.source {
        Some(_) => Some(provenance::verify(&store, turn, claim)?),
        None => None,
    };
    if changes.iter().any(|(path, _)| path == BOUNDARIES)
        && !matches!(&claim, Some(claim) if claim.kind == Kind::User)
    {
        let detail = match claim.as_ref().and_then(|claim| claim.code) {
            Some(code) => code.to_string(),
            None => "source user with the owner's approval is required".to_string(),
        };
        return Err(refused(E_MEMORY_QUOTE, detail));
    }
    let stamped = !matches!(
        (&claim, &edit),
        (_, Edit::Rename { .. } | Edit::Delete { .. }) | (None, Edit::Create { .. })
    );
    if stamped {
        for (_, text) in &mut changes {
            if let Some(text) = text {
                let new = match &claim {
                    Some(claim) => provenance::stamp(text, claim)?,
                    None => provenance::stamp_unclaimed(text)?,
                };
                if claim.is_some() || new != *text {
                    check_stamped(&new)?;
                }
                *text = new;
            }
        }
    }

    let commit = MemoryCommit {
        writer: WRITER_LEAD.to_string(),
        reason: reason.to_string(),
        turn_row: Some(turn.row_id),
        conversation_key: Some(turn.conversation_key.clone()),
        claim_json: claim.as_ref().map(Claim::to_json),
    };
    let id = history::record(&mut store, home, &commit, &changes)?;
    let soul_edit = changes
        .iter()
        .any(|(path, _)| SOUL_PATHS.contains(&path.as_str()));
    let tool = edit.tool();
    let (path, to) = match edit {
        Edit::Rename { old_path, new_path } => (old_path, Some(new_path)),
        other => (other.path().to_string(), None),
    };
    Ok(Applied {
        commit: id,
        tool,
        path,
        to,
        repaired: Vec::new(),
        claim,
        soul_edit,
    })
}

/// The running owner turn: `E_MEMORY_ORIGIN` when no turn runs or its row is not an owner
/// message; `E_MEMORY_HISTORY` when the row cannot be read.
pub(crate) fn owner_turn<'a>(
    store: &Store,
    turn: Option<&'a TurnContext>,
) -> Result<&'a TurnContext, MemoryError> {
    let Some(turn) = turn else {
        return Err(refused(E_MEMORY_ORIGIN, "no lead turn is running"));
    };
    match store.get(turn.row_id) {
        Ok(Some(row)) if row.origin == "owner" => Ok(turn),
        Ok(_) => Err(refused(
            E_MEMORY_ORIGIN,
            "the running turn does not answer an owner message",
        )),
        Err(e) => Err(refused(E_MEMORY_HISTORY, e.code())),
    }
}

/// The content checks of `history::check_text` for a text the engine stamped, without the
/// provenance comparison: the stamp may carry owner words that hold a sentinel or, once
/// escaped, match the secret table.
fn check_stamped(text: &str) -> Result<(), MemoryError> {
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(refused(E_MEMORY_FILE, "too large"));
    }
    if text.contains('\0') {
        return Err(refused(E_MEMORY_FILE, "contains NUL"));
    }
    if text.contains(BEGIN) || text.contains(END) {
        return Err(refused(E_MEMORY_FILE, "contains a memory block sentinel"));
    }
    parse(text)?;
    guard::refuse_secret("text", text)
}

/// Step 3: every named path is plain ASCII and passes the path rules, a path that would newly
/// hold text (the `Create` path, the `Rename` target) does not look like a secret, a rename
/// moves the file somewhere else, the persona and the owner card are neither deleted nor
/// renamed, and no named path is the reserved agreements path.
fn check_paths(home: &MemoryHome, edit: &Edit) -> Result<(), MemoryError> {
    let files: Vec<&str> = match edit {
        Edit::Delete { path } if !path.ends_with(".md") => {
            plain_ascii(path)?;
            home.resolve_dir(path)?;
            Vec::new()
        }
        Edit::Rename { old_path, new_path } => vec![old_path, new_path],
        other => vec![other.path()],
    };
    for path in files {
        plain_ascii(path)?;
        home.resolve(path)?;
    }
    match edit {
        Edit::Create { path, .. } => guard::refuse_secret("path", path)?,
        Edit::Rename { new_path, .. } => guard::refuse_secret("path", new_path)?,
        _ => {}
    }
    if let Edit::Rename { old_path, new_path } = edit
        && old_path == new_path
    {
        return Err(refused(E_MEMORY_NOOP, "the new path is the old path"));
    }
    let protected = match edit {
        Edit::Delete { path } if path.ends_with(".md") => PROTECTED.contains(&path.as_str()),
        Edit::Delete { path } => {
            let prefix = format!("{path}/");
            PROTECTED.iter().any(|p| p.starts_with(&prefix))
        }
        Edit::Rename { old_path, .. } => PROTECTED.contains(&old_path.as_str()),
        _ => false,
    };
    if protected {
        return Err(refused(
            E_MEMORY_PROTECTED,
            format!("{}: cannot be deleted or renamed", edit.path()),
        ));
    }
    let reserved = match edit {
        Edit::Rename { old_path, new_path } => {
            old_path == AGREEMENTS_FILE || new_path == AGREEMENTS_FILE
        }
        other => other.path() == AGREEMENTS_FILE,
    };
    if reserved {
        return Err(refused(
            E_MEMORY_AGREEMENT,
            format!("{AGREEMENTS_FILE} is reserved for the agreements ledger"),
        ));
    }
    Ok(())
}

/// Write paths use only ASCII letters, digits, '.', '_' and '-' in each non-empty component
/// (M2-D-ascii), so letter case is the only spelling difference a file system can fold, and
/// every path has one spelling in the history.
fn plain_ascii(path: &str) -> Result<(), MemoryError> {
    let plain = path.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    });
    if plain {
        Ok(())
    } else {
        Err(refused(
            E_MEMORY_PATH,
            "path components must be ASCII letters, digits, '.', '_' or '-'",
        ))
    }
}

/// Step 6: a new path is absent in any letter case, and every source is committed.
fn check_existence(
    home: &MemoryHome,
    committed: &Committed,
    edit: &Edit,
) -> Result<(), MemoryError> {
    match edit {
        Edit::Create { path, .. } => check_absent(home, committed, path),
        Edit::Rename { old_path, new_path } => {
            check_absent(home, committed, new_path)?;
            check_committed(committed, old_path)
        }
        Edit::Delete { path } if !path.ends_with(".md") => {
            if under(committed, path).is_empty() {
                Err(refused(
                    E_MEMORY_NOT_FOUND,
                    format!("{path}: no memory file in this folder"),
                ))
            } else {
                Ok(())
            }
        }
        other => check_committed(committed, other.path()),
    }
}

fn check_committed(committed: &Committed, path: &str) -> Result<(), MemoryError> {
    if committed.files.contains_key(path) {
        Ok(())
    } else {
        Err(refused(E_MEMORY_NOT_FOUND, path))
    }
}

/// Refuses a new path that names a committed file, a committed folder or a folder on disk in
/// another letter case: on a case-folding file system it would overwrite or split that entry.
fn check_absent(home: &MemoryHome, committed: &Committed, target: &str) -> Result<(), MemoryError> {
    let lower = target.to_lowercase();
    if let Some(existing) = committed.files.keys().find(|c| c.to_lowercase() == lower) {
        return Err(refused(
            E_MEMORY_EXISTS,
            format!("{target}: already a memory file as {existing}"),
        ));
    }
    let parts: Vec<&str> = target.split('/').collect();
    for committed_path in committed.files.keys() {
        let other: Vec<&str> = committed_path.split('/').collect();
        for depth in 1..parts.len().min(other.len()) {
            let (mine, theirs) = (parts[..depth].join("/"), other[..depth].join("/"));
            if mine != theirs && mine.to_lowercase() == theirs.to_lowercase() {
                return Err(refused(
                    E_MEMORY_EXISTS,
                    format!("{target}: folder committed as {theirs}"),
                ));
            }
        }
    }
    home.exact_spelling(target)
}

/// Step 7: no committed path the edit would change or remove is marked `read_only` in its
/// committed text. `Create` and the `Rename` target name uncommitted paths.
fn check_read_only(committed: &Committed, edit: &Edit) -> Result<(), MemoryError> {
    let paths: Vec<&str> = match edit {
        Edit::Create { .. } => Vec::new(),
        Edit::Delete { path } if !path.ends_with(".md") => under(committed, path)
            .into_iter()
            .map(String::as_str)
            .collect(),
        other => vec![other.path()],
    };
    match paths.into_iter().find(|path| {
        committed
            .files
            .get(*path)
            .is_some_and(|text| guard::is_read_only(text))
    }) {
        Some(path) => Err(refused(E_MEMORY_READ_ONLY, format!("{path}: read_only"))),
        None => Ok(()),
    }
}

/// The committed paths inside the folder `dir`.
fn under<'a>(committed: &'a Committed, dir: &str) -> Vec<&'a String> {
    let prefix = format!("{dir}/");
    committed
        .files
        .keys()
        .filter(|path| path.starts_with(&prefix))
        .collect()
}

/// Step 8: every change of `edit`, each with the text its `provenance` is compared against.
fn new_texts(
    committed: &Committed,
    edit: &Edit,
) -> Result<Vec<(MemoryChange, Option<String>)>, MemoryError> {
    let current = |path: &str| committed.files.get(path).cloned();
    let text_of = |path: &str| committed.files.get(path).map_or("", String::as_str);
    let changed = |path: &str, text: String| vec![((path.to_string(), Some(text)), current(path))];
    Ok(match edit {
        Edit::Create {
            path,
            description,
            body,
        } => {
            let mut text = render_frontmatter(&Frontmatter::with_description(description));
            text.push_str(body);
            if !body.is_empty() && !body.ends_with('\n') {
                text.push('\n');
            }
            changed(path, text)
        }
        Edit::StrReplace {
            path,
            old_string,
            new_string,
        } => {
            if old_string.is_empty() {
                return Err(refused(E_MEMORY_ARGS, "old_string is empty"));
            }
            let text = text_of(path);
            if !text.contains(old_string.as_str()) {
                return Err(refused(E_MEMORY_NO_MATCH, path.as_str()));
            }
            changed(path, text.replacen(old_string.as_str(), new_string, 1))
        }
        Edit::Insert {
            path,
            insert_line,
            insert_text,
        } => changed(path, insert(text_of(path), *insert_line, insert_text)?),
        Edit::UpdateDescription { path, description } => {
            let parsed = parse(text_of(path))?;
            let mut frontmatter = parsed.frontmatter;
            frontmatter.set("description", description);
            changed(path, render_frontmatter(&frontmatter) + parsed.body)
        }
        Edit::Rename { old_path, new_path } => {
            let text = current(old_path);
            vec![
                ((old_path.clone(), None), text.clone()),
                ((new_path.clone(), text.clone()), text),
            ]
        }
        Edit::Delete { path } if path.ends_with(".md") => {
            vec![((path.clone(), None), current(path))]
        }
        Edit::Delete { path } => under(committed, path)
            .into_iter()
            .map(|path| ((path.clone(), None), current(path)))
            .collect(),
    })
}

fn parse(text: &str) -> Result<frontmatter::Parsed<'_>, MemoryError> {
    frontmatter::parse(text).map_err(|e| refused(E_MEMORY_FRONTMATTER, e.to_string()))
}

/// `text` with `insert_text` starting at body line `line` (1 = top, line count + 1 = append);
/// the frontmatter is kept byte for byte.
fn insert(text: &str, line: usize, insert_text: &str) -> Result<String, MemoryError> {
    let body = parse(text)?.body;
    let head = &text[..text.len() - body.len()];
    let mut lines: Vec<String> = body.split_inclusive('\n').map(str::to_string).collect();
    if let Some(last) = lines.last_mut()
        && !last.ends_with('\n')
    {
        last.push('\n');
    }
    if line == 0 || line > lines.len() + 1 {
        return Err(refused(
            E_MEMORY_ARGS,
            format!("insert_line must be between 1 and {}", lines.len() + 1),
        ));
    }
    let mut added = insert_text.to_string();
    if !added.ends_with('\n') {
        added.push('\n');
    }
    lines.insert(line - 1, added);
    Ok(format!("{head}{}", lines.concat()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::memory::home::HUMAN_SEED;
    use crate::memory::provenance::{Kind, Source};
    use crate::memory::{
        E_MEMORY_PROVENANCE, E_MEMORY_QUOTE_APPROVAL, E_MEMORY_QUOTE_MISSING, E_MEMORY_SECRET,
        HUMAN, IDENTITY, MAX_REASON_CHARS, PERSONA, PROVENANCE_KEY,
    };
    use crate::store::{Admitted, LaneStart, LaneState, MemoryCommitRow};

    const A: &str = "---\ndescription: a note\n---\nline1\nline2\nline3\n";
    const B: &str = "---\ndescription: b note\n---\nno newline";
    const P: &str = "---\ndescription: kept\nprovenance: owner\n---\nbody\n";
    const REASON: &str = "test write";
    const OWNER_ROW: i64 = 1;
    const LANE_ROW: i64 = 2;
    const MISSING_ROW: i64 = 999;

    struct Fx {
        _dir: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        home: MemoryHome,
    }

    impl Fx {
        fn apply(&self, edit: Edit) -> Result<Applied, MemoryError> {
            self.claimed(edit, ClaimArgs::default())
        }

        fn claimed(&self, edit: Edit, claim: ClaimArgs) -> Result<Applied, MemoryError> {
            apply(
                &self.store,
                &self.home,
                Some(&turn(OWNER_ROW)),
                edit,
                REASON,
                &claim,
            )
        }

        /// The `provenance` value of a working file.
        fn provenance(&self, rel: &str) -> Option<String> {
            let text = self.file(rel).expect("file");
            let parsed = frontmatter::parse(&text).expect("frontmatter");
            parsed.frontmatter.get(PROVENANCE_KEY).map(str::to_owned)
        }

        /// The parsed `claim_json` of the last commit, if it has one.
        fn last_claim(&self) -> Option<serde_json::Value> {
            let last = self.commits().pop().expect("commit");
            last.commit
                .claim_json
                .map(|json| serde_json::from_str(&json).expect("claim json"))
        }

        /// Admits an owner row of `k1` by the turn row's author and returns its id.
        fn owner_row(&self, message_id: &str, text: &str) -> i64 {
            match lock(&self.store)
                .admit("test", message_id, "k1", "chan1", "111", text)
                .expect("admit")
            {
                Admitted::New(id) => id,
                Admitted::Duplicate => panic!("duplicate row"),
            }
        }

        fn commits(&self) -> Vec<MemoryCommitRow> {
            lock(&self.store).memory_commits().expect("commits")
        }

        fn file(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.home.root().join(rel)).ok()
        }

        fn dirty(&self) -> Vec<String> {
            let committed = history::committed(&lock(&self.store)).expect("committed");
            history::dirty(&self.home, &committed).expect("dirty")
        }
    }

    /// A home with the seeds and the notes a, b and p, imported, plus an owner row (id 1) and
    /// a lane-result row (id 2) in the inbox.
    fn fx() -> Fx {
        fx_with(&[])
    }

    /// `fx` with `extra` files written before the import.
    fn fx_with(extra: &[(&str, &str)]) -> Fx {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = Store::open(&dir.path().join("inbox.sqlite")).expect("store");
        assert_eq!(
            store
                .admit("test", "m1", "k1", "chan1", "111", "hi")
                .expect("admit"),
            Admitted::New(OWNER_ROW)
        );
        let LaneStart::New(lane) = store
            .start_lane("call-1", "k1", "task", None, 3)
            .expect("lane")
        else {
            panic!("lane not started");
        };
        assert!(
            store
                .finish_lane(lane, 1, "result", LaneState::Idle, None)
                .expect("finish")
        );
        let lane_row = store.get(LANE_ROW).expect("get").expect("lane row");
        assert_eq!(lane_row.origin, "lane");
        let home = MemoryHome::new(dir.path());
        home.ensure_layout().expect("layout");
        for (rel, text) in [("notes/a.md", A), ("notes/b.md", B), ("notes/p.md", P)] {
            put(&home, rel, text);
        }
        for (rel, text) in extra {
            put(&home, rel, text);
        }
        history::prepare(&mut store, &home).expect("import");
        Fx {
            _dir: dir,
            store: Arc::new(Mutex::new(store)),
            home,
        }
    }

    fn turn(row_id: i64) -> TurnContext {
        TurnContext {
            row_id,
            conversation_key: "k1".to_string(),
        }
    }

    fn put(home: &MemoryHome, rel: &str, text: &str) {
        let path = home.root().join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, text).expect("write");
    }

    fn create(path: &str, body: &str) -> Edit {
        Edit::Create {
            path: path.to_string(),
            description: "new note".to_string(),
            body: body.to_string(),
        }
    }

    fn replace(path: &str, old: &str, new: &str) -> Edit {
        Edit::StrReplace {
            path: path.to_string(),
            old_string: old.to_string(),
            new_string: new.to_string(),
        }
    }

    fn insert_at(path: &str, line: usize, text: &str) -> Edit {
        Edit::Insert {
            path: path.to_string(),
            insert_line: line,
            insert_text: text.to_string(),
        }
    }

    fn delete(path: &str) -> Edit {
        Edit::Delete {
            path: path.to_string(),
        }
    }

    fn rename(old: &str, new: &str) -> Edit {
        Edit::Rename {
            old_path: old.to_string(),
            new_path: new.to_string(),
        }
    }

    fn describe(path: &str, description: &str) -> Edit {
        Edit::UpdateDescription {
            path: path.to_string(),
            description: description.to_string(),
        }
    }

    /// Every file under `root` with its bytes.
    fn bytes_under(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
                let path = entry.path();
                let rel = path.strip_prefix(base).expect("prefix");
                let rel = rel.to_string_lossy().into_owned();
                if entry.file_type().expect("type").is_dir() {
                    walk(base, &path, out);
                } else {
                    out.insert(rel, std::fs::read(&path).expect("read"));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    type Expected = Vec<(&'static str, Option<String>)>;

    /// One case per edit kind: the edit, the resulting text of every changed path (`None` =
    /// gone), and the `path` and `to` of the result.
    fn cases() -> Vec<(Edit, Expected, &'static str, Option<&'static str>)> {
        let a = |body: &str| Some(format!("---\ndescription: a note\n---\n{body}"));
        vec![
            (
                create("notes/c.md", "hello"),
                vec![(
                    "notes/c.md",
                    Some("---\ndescription: new note\n---\nhello\n".to_string()),
                )],
                "notes/c.md",
                None,
            ),
            (
                replace("notes/a.md", "line2", "LINE2"),
                vec![("notes/a.md", a("line1\nLINE2\nline3\n"))],
                "notes/a.md",
                None,
            ),
            (
                insert_at("notes/a.md", 1, "top"),
                vec![("notes/a.md", a("top\nline1\nline2\nline3\n"))],
                "notes/a.md",
                None,
            ),
            (
                insert_at("notes/a.md", 2, "mid\n"),
                vec![("notes/a.md", a("line1\nmid\nline2\nline3\n"))],
                "notes/a.md",
                None,
            ),
            (
                insert_at("notes/a.md", 3, "before last"),
                vec![("notes/a.md", a("line1\nline2\nbefore last\nline3\n"))],
                "notes/a.md",
                None,
            ),
            (
                insert_at("notes/a.md", 4, "end"),
                vec![("notes/a.md", a("line1\nline2\nline3\nend\n"))],
                "notes/a.md",
                None,
            ),
            (
                insert_at("notes/b.md", 2, "end"),
                vec![(
                    "notes/b.md",
                    Some("---\ndescription: b note\n---\nno newline\nend\n".to_string()),
                )],
                "notes/b.md",
                None,
            ),
            (
                delete("notes/a.md"),
                vec![("notes/a.md", None)],
                "notes/a.md",
                None,
            ),
            (
                rename("notes/p.md", "notes/q.md"),
                vec![("notes/p.md", None), ("notes/q.md", Some(P.to_string()))],
                "notes/p.md",
                Some("notes/q.md"),
            ),
            (
                describe("notes/a.md", "changed"),
                vec![(
                    "notes/a.md",
                    Some("---\ndescription: changed\n---\nline1\nline2\nline3\n".to_string()),
                )],
                "notes/a.md",
                None,
            ),
        ]
    }

    #[test]
    fn each_edit_produces_expected_bytes() {
        for (edit, expected, path, to) in cases() {
            let fx = fx();
            let applied = fx.apply(edit.clone()).expect("apply");
            let last = fx.commits().pop().expect("commit");
            assert_eq!(
                applied,
                Applied {
                    commit: last.id,
                    tool: edit.tool(),
                    path: path.to_string(),
                    to: to.map(str::to_string),
                    repaired: vec![],
                    claim: None,
                    soul_edit: false,
                }
            );
            let json: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
            let mut want = serde_json::json!({
                "committed": last.id,
                "command": edit.tool(),
                "path": path,
            });
            if let Some(to) = to {
                want["to"] = serde_json::json!(to);
            }
            assert_eq!(json, want, "{edit:?}");
            for (rel, text) in expected {
                assert_eq!(fx.file(rel), text, "{edit:?} {rel}");
            }
            assert!(fx.dirty().is_empty(), "{edit:?}");
        }
    }

    fn soul_fx() -> Fx {
        fx_with(&[(PERSONA, A), (IDENTITY, A), ("notes/x.md", A)])
    }

    #[test]
    fn soul_edit_is_flagged_for_persona_and_identity() {
        for (edit, flagged) in [
            (replace(PERSONA, "line1", "changed"), true),
            (replace(IDENTITY, "line1", "changed"), true),
            (delete(IDENTITY), true),
            (replace("notes/x.md", "line1", "changed"), false),
        ] {
            let fx = soul_fx();
            let applied = fx.apply(edit.clone()).expect("apply");
            assert_eq!(applied.soul_edit, flagged, "{edit:?}");
            let json: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
            if flagged {
                assert_eq!(json["soul_edit"], true, "{edit:?}");
            } else {
                assert!(json.get("soul_edit").is_none(), "{json}");
            }
        }
        let fx = fx();
        let created = fx.apply(create(IDENTITY, "body")).expect("create identity");
        assert!(created.soul_edit);
        let json: serde_json::Value = serde_json::from_str(&created.to_json()).expect("json");
        assert_eq!(json["soul_edit"], true);
        let plain = fx.apply(create("notes/y.md", "body")).expect("create note");
        assert!(!plain.soul_edit);
        let json: serde_json::Value = serde_json::from_str(&plain.to_json()).expect("json");
        assert!(json.get("soul_edit").is_none(), "{json}");
    }

    #[test]
    fn rename_onto_identity_is_a_soul_edit() {
        let fx = fx_with(&[("notes/x.md", A)]);
        let applied = fx.apply(rename("notes/x.md", IDENTITY)).expect("rename");
        assert!(applied.soul_edit);
    }

    #[test]
    fn applied_json_lists_repaired_only_when_set() {
        let mut applied = Applied {
            commit: 7,
            tool: MEMORY_STR_REPLACE_TOOL,
            path: "notes/a.md".to_string(),
            to: None,
            repaired: vec![],
            claim: None,
            soul_edit: false,
        };
        let plain: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
        assert!(plain.get("repaired").is_none(), "{plain}");
        assert!(plain.get("source").is_none(), "{plain}");
        assert!(plain.get("soul_edit").is_none(), "{plain}");

        applied.repaired = vec!["new_string".to_string()];
        let json: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
        assert_eq!(json["repaired"], serde_json::json!(["new_string"]));
        assert_eq!(json["committed"], plain["committed"]);
        assert_eq!(json["command"], plain["command"]);
        assert_eq!(json["path"], plain["path"]);
        assert_eq!(json.as_object().expect("object").len(), 4, "{json}");

        applied.claim = Some(Claim::plain(Source::Agent));
        let json: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
        assert_eq!(json["source"], "agent");
        assert_eq!(json["downgraded"], false);
        assert!(json.get("code").is_none(), "{json}");
        assert_eq!(json.as_object().expect("object").len(), 6, "{json}");

        applied.claim = Some(Claim::plain(Source::User));
        let json: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
        assert_eq!(json["source"], "agent");
        assert_eq!(json["downgraded"], true);
        assert_eq!(json["code"], E_MEMORY_QUOTE_MISSING);
        assert_eq!(json.as_object().expect("object").len(), 7, "{json}");

        applied.soul_edit = true;
        let json: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
        assert_eq!(json["soul_edit"], true);
        assert_eq!(json.as_object().expect("object").len(), 8, "{json}");
    }

    #[test]
    fn each_write_records_one_commit_with_turn_fields() {
        for (edit, expected, _, _) in cases() {
            let fx = fx();
            let before = fx.commits().len();
            fx.apply(edit.clone()).expect("apply");
            let rows = fx.commits();
            assert_eq!(rows.len(), before + 1, "{edit:?}");
            let row = rows.last().expect("row");
            assert_eq!(
                row.commit,
                MemoryCommit {
                    writer: WRITER_LEAD.to_string(),
                    reason: REASON.to_string(),
                    turn_row: Some(OWNER_ROW),
                    conversation_key: Some("k1".to_string()),
                    claim_json: None,
                },
                "{edit:?}"
            );
            assert!(row.applied, "{edit:?}");
            let mut want: Vec<MemoryChange> = expected
                .into_iter()
                .map(|(rel, text)| (rel.to_string(), text))
                .collect();
            want.sort();
            let snapshots = lock(&fx.store).memory_snapshots(row.id).expect("snapshots");
            assert_eq!(snapshots, want, "{edit:?}");
        }
    }

    #[test]
    fn str_replace_changes_first_match_only() {
        let fx = fx();
        fx.apply(Edit::Create {
            path: "notes/t.md".to_string(),
            description: "t".to_string(),
            body: "a a".to_string(),
        })
        .expect("create");
        fx.apply(replace("notes/t.md", "a", "b")).expect("replace");
        assert_eq!(
            fx.file("notes/t.md").as_deref(),
            Some("---\ndescription: t\n---\nb a\n")
        );
    }

    #[test]
    fn update_description_keeps_other_keys_and_body() {
        let fx = fx();
        fx.apply(describe(HUMAN, "the owner")).expect("describe");
        let text = fx.file(HUMAN).expect("human");
        let parsed = frontmatter::parse(&text).expect("parse");
        let seed = frontmatter::parse(HUMAN_SEED).expect("seed");
        assert_eq!(
            seed.frontmatter.entries()[1],
            ("kind".to_string(), "person".to_string())
        );
        assert_eq!(
            parsed.frontmatter.entries(),
            [
                ("description".to_string(), "the owner".to_string()),
                ("kind".to_string(), "person".to_string()),
            ]
        );
        assert_eq!(parsed.body, seed.body);
    }

    #[test]
    fn delete_folder_removes_every_tracked_file_in_one_commit() {
        let fx = fx();
        fx.apply(create("notes/x/1.md", "one")).expect("one");
        fx.apply(create("notes/x/2.md", "two")).expect("two");
        let before = fx.commits().len();
        let applied = fx.apply(delete("notes/x")).expect("delete folder");
        let rows = fx.commits();
        assert_eq!(rows.len(), before + 1);
        assert_eq!(applied.commit, rows[before].id);
        assert_eq!(applied.path, "notes/x");
        assert_eq!(
            lock(&fx.store)
                .memory_snapshots(applied.commit)
                .expect("snapshots"),
            vec![
                ("notes/x/1.md".to_string(), None),
                ("notes/x/2.md".to_string(), None),
            ]
        );
        assert_eq!(fx.file("notes/x/1.md"), None);
        assert_eq!(fx.file("notes/x/2.md"), None);
        assert!(fx.dirty().is_empty());
    }

    /// Asserts that the edit is refused with `code` and that neither a memory file nor the
    /// commit count changed; returns the refusal.
    fn assert_refused(
        fx: &Fx,
        row: Option<i64>,
        edit: Edit,
        reason: &str,
        code: &str,
    ) -> MemoryError {
        assert_claim_refused(fx, row, edit, reason, &ClaimArgs::default(), code)
    }

    /// `assert_refused` for an edit that carries `claim`.
    fn assert_claim_refused(
        fx: &Fx,
        row: Option<i64>,
        edit: Edit,
        reason: &str,
        claim: &ClaimArgs,
        code: &str,
    ) -> MemoryError {
        let before = (bytes_under(fx.home.root()), fx.commits().len());
        let turn = row.map(turn);
        let err = apply(
            &fx.store,
            &fx.home,
            turn.as_ref(),
            edit.clone(),
            reason,
            claim,
        )
        .expect_err(code);
        assert_eq!(err.code, code, "{edit:?}");
        let after = (bytes_under(fx.home.root()), fx.commits().len());
        assert_eq!(after, before, "{code} {edit:?}");
        err
    }

    /// A secret-like value built at run time: `parts` joined, then `n` copies of `fill`, so no
    /// source line holds a whole token.
    fn fake(parts: &[&str], fill: char, n: usize) -> String {
        let mut out = parts.concat();
        out.extend(std::iter::repeat_n(fill, n));
        out
    }

    fn sk_key() -> String {
        fake(&["sk", "-"], 'a', 24)
    }

    fn github_token() -> String {
        fake(&["gh", "p_"], 'A', 36)
    }

    fn private_key_header() -> String {
        ["-----BEGIN ", "RSA PRIVATE", " KEY-----"].concat()
    }

    /// Asserts that `err` is a secret refusal of `class` whose detail does not hold the
    /// matched bytes of `secret`.
    fn assert_no_echo(err: &MemoryError, secret: &str, class: guard::SecretClass) {
        assert_eq!(err.code, E_MEMORY_SECRET);
        let found = guard::find_secret(secret).expect("a secret");
        assert_eq!(found.class, class);
        assert!(!err.detail.contains(&secret[found.start..found.end]));
        assert!(err.detail.contains(class.name()), "{}", class.name());
    }

    const RO: &str = "---\ndescription: locked\nread_only: \"true\"\n---\nbody\n";
    const RW: &str = "---\ndescription: open\nread_only: \"false\"\n---\nbody\n";

    /// Every edit of `path` the read_only step guards, the folder delete aside.
    fn edits_of(path: &str) -> Vec<Edit> {
        vec![
            replace(path, "body", "changed"),
            insert_at(path, 1, "top"),
            describe(path, "changed"),
            delete(path),
            rename(path, "notes/moved.md"),
        ]
    }

    #[test]
    fn read_only_file_refuses_every_write() {
        let files = [("notes/ro.md", RO), ("notes/rw.md", RW)];
        let mut locked = edits_of("notes/ro.md");
        locked.push(delete("notes"));
        for edit in locked {
            let fx = fx_with(&files);
            let err = assert_refused(&fx, Some(OWNER_ROW), edit, REASON, E_MEMORY_READ_ONLY);
            assert_eq!(err.detail, "notes/ro.md: read_only");
        }
        for edit in edits_of("notes/rw.md") {
            let fx = fx_with(&files);
            let before = fx.commits().len();
            fx.apply(edit.clone()).expect("unlocked write");
            assert_eq!(fx.commits().len(), before + 1, "{edit:?}");
        }
    }

    #[test]
    fn read_only_can_be_set_but_not_removed() {
        let fx = fx();
        let open = "description: a note\n";
        let locked = "description: a note\nread_only: \"true\"\n";
        fx.apply(replace("notes/a.md", open, locked)).expect("lock");
        assert!(guard::is_read_only(&fx.file("notes/a.md").expect("a")));
        assert_refused(
            &fx,
            Some(OWNER_ROW),
            replace("notes/a.md", locked, open),
            REASON,
            E_MEMORY_READ_ONLY,
        );
    }

    #[test]
    fn secret_writes_are_refused_without_echo() {
        use guard::SecretClass;
        let (sk, pem, gh) = (sk_key(), private_key_header(), github_token());
        let slack = fake(&["xo", "xb-"], '1', 12);
        let sk_path = format!("notes/{sk}.md");
        let cases = [
            (
                create("notes/c.md", &format!("key {sk}")),
                REASON.to_string(),
                sk.clone(),
                SecretClass::SkKey,
            ),
            (
                replace("notes/a.md", "line2", &pem),
                REASON.to_string(),
                pem.clone(),
                SecretClass::PrivateKey,
            ),
            (
                describe("notes/a.md", &gh),
                REASON.to_string(),
                gh.clone(),
                SecretClass::GithubToken,
            ),
            (
                create(&sk_path, "x"),
                REASON.to_string(),
                sk_path.clone(),
                SecretClass::SkKey,
            ),
            (
                create("notes/c.md", "x"),
                format!("note {slack}"),
                slack.clone(),
                SecretClass::SlackToken,
            ),
        ];
        for (edit, reason, secret, class) in cases {
            let fx = fx();
            let err = assert_refused(&fx, Some(OWNER_ROW), edit, &reason, E_MEMORY_SECRET);
            assert_no_echo(&err, &secret, class);
        }
    }

    #[test]
    fn old_secret_must_be_removed_first() {
        let token = fake(&["gl", "pat-"], 'a', 20);
        let text = format!("---\ndescription: g\n---\nline1\n{token}\n");
        let fx = fx_with(&[("notes/g.md", &text)]);
        let err = assert_refused(
            &fx,
            Some(OWNER_ROW),
            replace("notes/g.md", "line1", "LINE1"),
            REASON,
            E_MEMORY_SECRET,
        );
        assert_no_echo(&err, &token, guard::SecretClass::GitlabToken);
        fx.apply(replace("notes/g.md", &token, "removed"))
            .expect("remove the secret");
        assert_eq!(
            fx.file("notes/g.md").as_deref(),
            Some("---\ndescription: g\n---\nline1\nremoved\n")
        );
    }

    #[test]
    fn secret_like_path_can_be_removed() {
        let sk_path = format!(
            "notes/{}.md",
            fake(&["sk", "-", "learn-pipeline-experiments"], 'x', 0)
        );
        let gh_path = format!("notes/{}.md", github_token());
        assert!(guard::find_secret(&sk_path).is_some());
        assert!(guard::find_secret(&gh_path).is_some());
        let fx = fx_with(&[(sk_path.as_str(), A), (gh_path.as_str(), A)]);
        let count = fx.commits().len();
        fx.apply(replace(&sk_path, "line1", "LINE1"))
            .expect("edit in place");
        fx.apply(delete(&sk_path)).expect("delete");
        fx.apply(rename(&gh_path, "notes/plain.md"))
            .expect("rename away");
        assert_eq!(fx.commits().len(), count + 3);
        assert_eq!(fx.file(&sk_path), None);
        assert_eq!(fx.file(&gh_path), None);
        assert_eq!(fx.file("notes/plain.md").as_deref(), Some(A));
        let new_path = format!("notes/{}.md", sk_key());
        assert_refused(
            &fx,
            Some(OWNER_ROW),
            create(&new_path, "x"),
            REASON,
            E_MEMORY_SECRET,
        );
        assert_refused(
            &fx,
            Some(OWNER_ROW),
            rename("notes/plain.md", &gh_path),
            REASON,
            E_MEMORY_SECRET,
        );
    }

    #[test]
    fn refusals_carry_codes_and_change_nothing() {
        type Setup = fn(&Fx);
        fn none(_: &Fx) {}
        fn hand_edit(fx: &Fx) {
            put(&fx.home, "notes/a.md", "---\ndescription: by hand\n---\n");
        }
        fn committed_x(fx: &Fx) {
            fx.apply(create("notes/x/1.md", "one")).expect("x/1");
        }
        fn empty_bob(fx: &Fx) {
            fx.apply(create("people/bob/c.md", "c")).expect("bob");
            fx.apply(delete("people/bob/c.md")).expect("delete bob");
            assert!(fx.home.root().join("people/bob").is_dir());
        }
        fn empty_folder(fx: &Fx) {
            std::fs::create_dir_all(fx.home.root().join("notes/empty")).expect("mkdir");
        }
        let owner = Some(OWNER_ROW);
        let ok = || REASON.to_string();
        let big = "a".repeat(MAX_FILE_BYTES as usize);
        let provenance = replace(
            "notes/a.md",
            "description: a note\n",
            "description: a note\nprovenance: x\n",
        );
        let unfenced = replace("notes/a.md", "---\nline1", "line1");
        let cases: Vec<(Setup, Option<i64>, Edit, String, &str)> = vec![
            (
                none,
                owner,
                create("notes/c.md", "x"),
                String::new(),
                E_MEMORY_ARGS,
            ),
            (
                none,
                owner,
                create("notes/c.md", "x"),
                "a\nb".to_string(),
                E_MEMORY_ARGS,
            ),
            (
                none,
                owner,
                create("notes/c.md", "x"),
                "a".repeat(MAX_REASON_CHARS + 1),
                E_MEMORY_ARGS,
            ),
            (
                none,
                owner,
                replace("notes/a.md", "", "x"),
                ok(),
                E_MEMORY_ARGS,
            ),
            (
                none,
                owner,
                insert_at("notes/a.md", 5, "x"),
                ok(),
                E_MEMORY_ARGS,
            ),
            (none, None, create("notes/c.md", "x"), ok(), E_MEMORY_ORIGIN),
            (
                none,
                Some(LANE_ROW),
                create("notes/c.md", "x"),
                ok(),
                E_MEMORY_ORIGIN,
            ),
            (
                none,
                Some(MISSING_ROW),
                create("notes/c.md", "x"),
                ok(),
                E_MEMORY_ORIGIN,
            ),
            (none, owner, create("../x.md", "x"), ok(), E_MEMORY_PATH),
            (none, owner, create("other/x.md", "x"), ok(), E_MEMORY_PATH),
            (
                none,
                owner,
                create("notes/caf\u{e9}.md", "x"),
                ok(),
                E_MEMORY_PATH,
            ),
            (
                none,
                owner,
                create("notes/a b.md", "x"),
                ok(),
                E_MEMORY_PATH,
            ),
            (
                none,
                owner,
                rename("notes/a.md", "notes/\u{e9}.md"),
                ok(),
                E_MEMORY_PATH,
            ),
            (none, owner, delete(PERSONA), ok(), E_MEMORY_PROTECTED),
            (
                none,
                owner,
                rename(HUMAN, "system/h2.md"),
                ok(),
                E_MEMORY_PROTECTED,
            ),
            (none, owner, delete("system"), ok(), E_MEMORY_PROTECTED),
            (
                hand_edit,
                owner,
                create("notes/c.md", "x"),
                ok(),
                E_MEMORY_DIRTY,
            ),
            (
                none,
                owner,
                create("notes/a.md", "x"),
                ok(),
                E_MEMORY_EXISTS,
            ),
            (
                none,
                owner,
                rename("notes/b.md", "notes/a.md"),
                ok(),
                E_MEMORY_EXISTS,
            ),
            (
                none,
                owner,
                create("notes/A.md", "x"),
                ok(),
                E_MEMORY_EXISTS,
            ),
            (
                none,
                owner,
                rename("notes/a.md", "notes/A.md"),
                ok(),
                E_MEMORY_EXISTS,
            ),
            (
                none,
                owner,
                rename("notes/b.md", "notes/A.md"),
                ok(),
                E_MEMORY_EXISTS,
            ),
            (
                committed_x,
                owner,
                create("notes/X/2.md", "x"),
                ok(),
                E_MEMORY_EXISTS,
            ),
            (
                committed_x,
                owner,
                rename("notes/a.md", "notes/X/2.md"),
                ok(),
                E_MEMORY_EXISTS,
            ),
            (
                empty_bob,
                owner,
                create("people/Bob/c.md", "x"),
                ok(),
                E_MEMORY_EXISTS,
            ),
            (
                none,
                owner,
                replace("notes/missing.md", "a", "b"),
                ok(),
                E_MEMORY_NOT_FOUND,
            ),
            (
                empty_folder,
                owner,
                delete("notes/empty"),
                ok(),
                E_MEMORY_NOT_FOUND,
            ),
            (
                none,
                owner,
                replace("notes/a.md", "zzz", "y"),
                ok(),
                E_MEMORY_NO_MATCH,
            ),
            (none, owner, unfenced, ok(), E_MEMORY_FRONTMATTER),
            (none, owner, provenance, ok(), E_MEMORY_PROVENANCE),
            (none, owner, create("notes/c.md", &big), ok(), E_MEMORY_FILE),
            (none, owner, create("notes/c.md", END), ok(), E_MEMORY_FILE),
            (
                none,
                owner,
                create("notes/c.md", "a\0b"),
                ok(),
                E_MEMORY_FILE,
            ),
            (
                none,
                owner,
                replace("notes/a.md", "line1", "line1"),
                ok(),
                E_MEMORY_NOOP,
            ),
            (
                none,
                owner,
                rename("notes/a.md", "notes/a.md"),
                ok(),
                E_MEMORY_NOOP,
            ),
        ];
        for (setup, row, edit, reason, code) in cases {
            let fx = fx();
            setup(&fx);
            assert_refused(&fx, row, edit, &reason, code);
        }
    }

    #[test]
    fn refusal_order_is_fixed() {
        let fx = fx_with(&[("notes/ro.md", RO)]);
        let owner = Some(OWNER_ROW);
        let secret_path = format!("notes/{}.md", sk_key());
        let secret_reason = format!("note {}", fake(&["AK", "IA"], 'A', 16));
        for edit in [
            replace("notes/ro.md", "zzz", "y"),
            replace("notes/ro.md", "body", &private_key_header()),
            replace("notes/ro.md", "body", "body"),
        ] {
            assert_refused(&fx, owner, edit, REASON, E_MEMORY_READ_ONLY);
        }
        assert_refused(
            &fx,
            Some(LANE_ROW),
            create(&secret_path, "x"),
            REASON,
            E_MEMORY_ORIGIN,
        );
        assert_refused(
            &fx,
            None,
            create("notes/c.md", "x"),
            &secret_reason,
            E_MEMORY_SECRET,
        );
        assert_refused(
            &fx,
            Some(LANE_ROW),
            create("other/x.md", "x"),
            REASON,
            E_MEMORY_ORIGIN,
        );
        put(&fx.home, "notes/a.md", "---\ndescription: by hand\n---\n");
        assert_refused(
            &fx,
            owner,
            create(&secret_path, "x"),
            REASON,
            E_MEMORY_SECRET,
        );
        assert_refused(
            &fx,
            Some(OWNER_ROW),
            replace("notes/missing.md", "a", "b"),
            REASON,
            E_MEMORY_DIRTY,
        );
        assert_refused(&fx, None, create("notes/c.md", "x"), "", E_MEMORY_ARGS);

        let secret_quote = user(&format!("ok {}", sk_key()), OWNER_ROW);
        assert_claim_refused(
            &fx,
            None,
            create("notes/c.md", "x"),
            REASON,
            &secret_quote,
            E_MEMORY_SECRET,
        );
        let err = assert_claim_refused(
            &fx,
            owner,
            create("notes/c.md", "x"),
            &secret_reason,
            &secret_quote,
            E_MEMORY_SECRET,
        );
        assert!(err.detail.starts_with("reason:"), "{}", err.detail);
        let fx = fx_with(&[("notes/ro.md", RO)]);
        assert_claim_refused(
            &fx,
            owner,
            replace("notes/ro.md", "body", "changed"),
            REASON,
            &source(Source::Agent),
            E_MEMORY_READ_ONLY,
        );
    }

    fn source(source: Source) -> ClaimArgs {
        ClaimArgs {
            source: Some(source),
            ..ClaimArgs::default()
        }
    }

    fn user(quote: &str, message: i64) -> ClaimArgs {
        ClaimArgs {
            source: Some(Source::User),
            user_quote: Some(quote.to_string()),
            quote_message: Some(message),
        }
    }

    /// The kind word of a `provenance` value: the text before its first `:`.
    fn kind_of(value: Option<String>) -> String {
        let value = value.expect("provenance");
        value.split_once(':').expect("kind prefix").0.to_string()
    }

    #[test]
    fn no_source_keeps_the_m3_bytes() {
        let fx = fx();
        let applied = fx.apply(create("notes/c.md", "hello\n")).expect("create");
        assert_eq!(
            fx.file("notes/c.md"),
            Some(render_frontmatter(&Frontmatter::with_description("new note")) + "hello\n")
        );
        assert_eq!(fx.last_claim(), None);
        assert_eq!(applied.claim, None);
        let json: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
        assert!(json.get("source").is_none(), "{json}");
        assert!(json.get("downgraded").is_none(), "{json}");
    }

    #[test]
    fn agent_and_inferred_claims_are_stamped() {
        let fx = fx();
        let applied = fx
            .claimed(create("notes/c.md", "tea"), source(Source::Agent))
            .expect("create");
        assert_eq!(applied.claim, Some(Claim::plain(Source::Agent)));
        assert_eq!(kind_of(fx.provenance("notes/c.md")), "agent");
        let claim = fx.last_claim().expect("claim");
        assert_eq!(claim["source"], "agent");
        assert_eq!(claim["downgraded"], false);
        fx.claimed(
            replace("notes/c.md", "tea", "green tea"),
            source(Source::Inferred),
        )
        .expect("replace");
        assert_eq!(kind_of(fx.provenance("notes/c.md")), "inferred");
        assert_eq!(fx.last_claim().expect("claim")["source"], "inferred");
        assert!(fx.dirty().is_empty());
    }

    #[test]
    fn quoted_claim_then_agent_edit_is_mixed() {
        let fx = fx();
        let applied = fx
            .claimed(create("notes/c.md", "tea"), user("hi", OWNER_ROW))
            .expect("create");
        let claim = applied.claim.expect("claim");
        assert_eq!(claim.kind, Kind::Quoted);
        assert_eq!(claim.code, Some(E_MEMORY_QUOTE_APPROVAL));
        assert_eq!(kind_of(fx.provenance("notes/c.md")), "quoted");
        let stored = fx.last_claim().expect("claim");
        assert_eq!(stored["source"], "quoted");
        assert_eq!(stored["code"], E_MEMORY_QUOTE_APPROVAL);
        assert_eq!(stored["downgraded"], true);
        fx.claimed(insert_at("notes/c.md", 1, "more"), source(Source::Agent))
            .expect("insert");
        assert_eq!(kind_of(fx.provenance("notes/c.md")), "mixed");
    }

    #[test]
    fn legacy_provenance_becomes_mixed() {
        let fx = fx();
        fx.claimed(replace("notes/p.md", "body", "BODY"), source(Source::Agent))
            .expect("claimed replace");
        assert_eq!(kind_of(fx.provenance("notes/p.md")), "mixed");

        let fx = fx_with(&[]);
        fx.apply(replace("notes/p.md", "body", "BODY"))
            .expect("plain replace");
        assert_eq!(
            fx.file("notes/p.md").as_deref(),
            Some("---\ndescription: kept\nprovenance: owner\n---\nBODY\n")
        );
        assert_eq!(fx.last_claim(), None);
    }

    #[test]
    fn rename_and_delete_record_claim_without_stamp() {
        let fx = fx();
        fx.claimed(rename("notes/p.md", "notes/q.md"), source(Source::Agent))
            .expect("rename");
        assert_eq!(fx.file("notes/q.md").as_deref(), Some(P));
        assert_eq!(fx.last_claim().expect("claim")["source"], "agent");
        fx.claimed(delete("notes/a.md"), source(Source::Inferred))
            .expect("delete");
        assert_eq!(fx.file("notes/a.md"), None);
        assert_eq!(fx.last_claim().expect("claim")["source"], "inferred");
        assert!(fx.dirty().is_empty());
    }

    #[test]
    fn stamp_never_makes_a_noop_a_write() {
        let fx = fx();
        assert_claim_refused(
            &fx,
            Some(OWNER_ROW),
            replace("notes/a.md", "line1", "line1"),
            REASON,
            &source(Source::Agent),
            E_MEMORY_NOOP,
        );
    }

    #[test]
    fn claim_refusals_change_nothing() {
        let owner = Some(OWNER_ROW);
        let sk = sk_key();
        let fx = fx();
        let err = assert_claim_refused(
            &fx,
            owner,
            create("notes/c.md", "x"),
            REASON,
            &user(&format!("ok {sk}"), OWNER_ROW),
            E_MEMORY_SECRET,
        );
        assert_no_echo(&err, &sk, guard::SecretClass::SkKey);

        let head = render_frontmatter(&Frontmatter::with_description("new note"));
        let body = "a".repeat(MAX_FILE_BYTES as usize - head.len() - 1) + "\n";
        let fx = fx_with(&[]);
        assert_claim_refused(
            &fx,
            owner,
            create("notes/c.md", &body),
            REASON,
            &source(Source::Agent),
            E_MEMORY_FILE,
        );
        fx.apply(create("notes/c.md", &body))
            .expect("fits without a stamp");
        assert_eq!(
            fx.file("notes/c.md").map(|t| t.len() as u64),
            Some(MAX_FILE_BYTES)
        );

        // quoted_sentinel_is_refused
        let fx = fx_with(&[]);
        let row = fx.owner_row("m3", &format!("see {END} here"));
        assert_claim_refused(
            &fx,
            Some(row),
            create("notes/c.md", "x"),
            REASON,
            &user(END, row),
            E_MEMORY_FILE,
        );

        // escaped_quote_secret_is_refused
        let fx = fx_with(&[]);
        let quote = ["tok", "en="].concat() + "\n" + "abcdefgh";
        assert!(guard::find_secret(&quote).is_none());
        let row = fx.owner_row("m4", &format!("note {quote}"));
        assert_claim_refused(
            &fx,
            Some(row),
            create("notes/c.md", "x"),
            REASON,
            &user(&quote, row),
            E_MEMORY_SECRET,
        );
    }

    #[test]
    fn unclaimed_edit_of_owner_words_is_mixed() {
        let fx = fx();
        fx.claimed(create("notes/c.md", "tea"), user("hi", OWNER_ROW))
            .expect("create");
        assert_eq!(kind_of(fx.provenance("notes/c.md")), "quoted");
        let applied = fx
            .apply(replace("notes/c.md", "tea", "coffee"))
            .expect("replace");
        assert_eq!(kind_of(fx.provenance("notes/c.md")), "mixed");
        let text = fx.file("notes/c.md").expect("file");
        assert_eq!(frontmatter::parse(&text).expect("parse").body, "coffee\n");
        assert_eq!(fx.last_claim(), None);
        let json: serde_json::Value = serde_json::from_str(&applied.to_json()).expect("json");
        assert!(json.get("source").is_none(), "{json}");

        fx.claimed(create("notes/d.md", "tea"), source(Source::Agent))
            .expect("agent create");
        let before = fx.file("notes/d.md").expect("file");
        fx.apply(describe("notes/d.md", "changed"))
            .expect("describe");
        assert_eq!(
            fx.file("notes/d.md"),
            Some(before.replacen("description: new note", "description: changed", 1))
        );
        assert_eq!(kind_of(fx.provenance("notes/d.md")), "agent");
    }

    /// An owner turn that approves a proposal: an owner row of `k1` answered with a reply,
    /// then the approval row `yes`. Returns the turn on the approval row and its id.
    fn approved_turn(fx: &Fx) -> (TurnContext, i64) {
        let n = lock(&fx.store).count().expect("count");
        let proposal = fx.owner_row(&format!("m-proposal-{n}"), "please remember");
        {
            let mut store = lock(&fx.store);
            assert!(store.claim(proposal).expect("claim"));
            store
                .finish_group(proposal, "proposal-text", None, None)
                .expect("finish");
        }
        let approval = fx.owner_row(&format!("m-approval-{n}"), "yes");
        (turn(approval), approval)
    }

    const BOUNDARY_BODY: &str = "- never force push\n";

    /// Creates `system/boundaries.md` with a verified owner approval.
    fn verified_boundaries(fx: &Fx) -> Applied {
        let (turn, row) = approved_turn(fx);
        apply(
            &fx.store,
            &fx.home,
            Some(&turn),
            create(BOUNDARIES, BOUNDARY_BODY),
            REASON,
            &user("yes", row),
        )
        .expect("verified boundaries write")
    }

    #[test]
    fn boundaries_need_a_verified_owner_quote() {
        let fx = fx();
        let owner = Some(OWNER_ROW);
        let no_quote = ClaimArgs {
            source: Some(Source::User),
            ..ClaimArgs::default()
        };
        for (claim, detail) in [
            (
                ClaimArgs::default(),
                "source user with the owner's approval is required",
            ),
            (
                source(Source::Agent),
                "source user with the owner's approval is required",
            ),
            (
                source(Source::Inferred),
                "source user with the owner's approval is required",
            ),
            (no_quote, E_MEMORY_QUOTE_MISSING),
        ] {
            let err = assert_claim_refused(
                &fx,
                owner,
                create(BOUNDARIES, BOUNDARY_BODY),
                REASON,
                &claim,
                E_MEMORY_QUOTE,
            );
            assert_eq!(err.detail, detail, "{claim:?}");
        }

        let never = fx.owner_row("m-never", "never force push");
        assert!(lock(&fx.store).claim(never).expect("claim"));
        let quoted = user("never", never);
        let claim = provenance::verify(&lock(&fx.store), &turn(never), &quoted).expect("verify");
        assert_eq!(claim.kind, Kind::Quoted);
        assert_eq!(claim.code, Some(E_MEMORY_QUOTE_APPROVAL));
        let err = assert_claim_refused(
            &fx,
            Some(never),
            create(BOUNDARIES, BOUNDARY_BODY),
            REASON,
            &quoted,
            E_MEMORY_QUOTE,
        );
        assert_eq!(err.detail, E_MEMORY_QUOTE_APPROVAL);
        assert_eq!(fx.file(BOUNDARIES), None);

        let before = fx.commits().len();
        let applied = verified_boundaries(&fx);
        assert!(applied.soul_edit);
        assert_eq!(fx.commits().len(), before + 1);
        assert!(
            fx.provenance(BOUNDARIES)
                .expect("provenance")
                .starts_with("user:")
        );
        assert!(fx.dirty().is_empty());
    }

    #[test]
    fn boundaries_changes_and_removal_need_the_quote_too() {
        let fx = fx();
        verified_boundaries(&fx);
        let before = fx.file(BOUNDARIES).expect("boundaries");
        for edit in [
            replace(BOUNDARIES, "never", "always"),
            insert_at(BOUNDARIES, 1, "- never skip tests"),
            describe(BOUNDARIES, "changed"),
            delete(BOUNDARIES),
            rename(BOUNDARIES, "notes/moved.md"),
        ] {
            assert_refused(&fx, Some(OWNER_ROW), edit, REASON, E_MEMORY_QUOTE);
            assert_eq!(fx.file(BOUNDARIES).as_deref(), Some(before.as_str()));
        }
        assert_eq!(fx.file("notes/moved.md"), None);

        let (turn, row) = approved_turn(&fx);
        let applied = apply(
            &fx.store,
            &fx.home,
            Some(&turn),
            replace(BOUNDARIES, "never", "always"),
            REASON,
            &user("yes", row),
        )
        .expect("verified replace");
        assert!(applied.soul_edit);
        let text = fx.file(BOUNDARIES).expect("boundaries");
        assert_eq!(
            frontmatter::parse(&text).expect("parse").body,
            "- always force push\n"
        );

        let fx = fx_with(&[("notes/x.md", A)]);
        assert_eq!(fx.file(BOUNDARIES), None);
        assert_refused(
            &fx,
            Some(OWNER_ROW),
            rename("notes/x.md", BOUNDARIES),
            REASON,
            E_MEMORY_QUOTE,
        );
    }

    #[test]
    fn agreements_file_is_reserved() {
        let fx = fx_with(&[("notes/x.md", A)]);
        for edit in [
            create(AGREEMENTS_FILE, "- never force push\n"),
            rename("notes/x.md", AGREEMENTS_FILE),
        ] {
            let err = assert_refused(&fx, Some(OWNER_ROW), edit, REASON, E_MEMORY_AGREEMENT);
            assert_eq!(
                err.detail,
                "system/agreements.md is reserved for the agreements ledger"
            );
        }
        assert_eq!(fx.file(AGREEMENTS_FILE), None);
    }

    #[test]
    fn owner_turn_gate_is_shared() {
        let fx = fx();
        let store = lock(&fx.store);
        let err = owner_turn(&store, None).expect_err("no turn");
        assert_eq!(
            (err.code, err.detail.as_str()),
            (E_MEMORY_ORIGIN, "no lead turn is running")
        );
        let lane = turn(LANE_ROW);
        let err = owner_turn(&store, Some(&lane)).expect_err("lane turn");
        assert_eq!(
            (err.code, err.detail.as_str()),
            (
                E_MEMORY_ORIGIN,
                "the running turn does not answer an owner message"
            )
        );
        let missing = turn(MISSING_ROW);
        let err = owner_turn(&store, Some(&missing)).expect_err("missing row");
        assert_eq!(err.code, E_MEMORY_ORIGIN);
        let owner = turn(OWNER_ROW);
        let got = owner_turn(&store, Some(&owner)).expect("owner turn");
        assert!(std::ptr::eq(got, &owner));
    }

    /// Two owner-turn writers released together by a barrier, each creating one of `paths`.
    fn race(fx: &Fx, paths: [&str; 2]) -> Vec<Result<Applied, MemoryError>> {
        let barrier = Arc::new(Barrier::new(2));
        let writers: Vec<_> = paths
            .iter()
            .map(|path| {
                let (store, home) = (Arc::clone(&fx.store), fx.home.clone());
                let (barrier, edit) = (Arc::clone(&barrier), create(path, "x"));
                std::thread::spawn(move || {
                    barrier.wait();
                    apply(
                        &store,
                        &home,
                        Some(&turn(OWNER_ROW)),
                        edit,
                        REASON,
                        &ClaimArgs::default(),
                    )
                })
            })
            .collect();
        writers
            .into_iter()
            .map(|writer| writer.join().expect("writer"))
            .collect()
    }

    #[test]
    fn two_writers_at_a_barrier_make_two_commits() {
        let fx = fx();
        let before = fx.commits().len();
        let ids: Vec<i64> = race(&fx, ["notes/r1.md", "notes/r2.md"])
            .into_iter()
            .map(|result| result.expect("both commit").commit)
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert_eq!(fx.commits().len(), before + 2);
        assert!(fx.file("notes/r1.md").is_some());
        assert!(fx.file("notes/r2.md").is_some());
        assert!(fx.dirty().is_empty());
    }

    #[test]
    fn same_path_race_commits_once() {
        let fx = fx();
        let before = fx.commits().len();
        let results = race(&fx, ["notes/same.md", "notes/same.md"]);
        let committed = results.iter().filter(|result| result.is_ok()).count();
        let codes: Vec<&str> = results
            .iter()
            .filter_map(|result| result.as_ref().err().map(|e| e.code))
            .collect();
        assert_eq!(committed, 1);
        assert_eq!(codes, [E_MEMORY_EXISTS]);
        assert_eq!(fx.commits().len(), before + 1);
    }
}
