//! What changed since the lead's block was pinned, and the notice lines that say so.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::history::{self, Committed};
use super::home::MemoryError;
use super::{
    E_MEMORY_HISTORY, EXPIRES_KEY, MAX_NOTICE_PATHS, MAX_ONBOARDING_TURNS, MEMORY_DIR,
    NOTICE_BEGIN, NOTICE_END, NUDGE_EVERY, REFRESH_FILE, SOUL_PATHS, WRITER_LEAD, frontmatter,
    guard,
};
use crate::store::{AgreementRow, MemoryChangeRow, Store, StoreError};

/// Unix seconds now; tests inject a fixed or stepped clock.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The wall clock (`guard::unix_now`).
pub fn system_clock() -> Clock {
    Arc::new(guard::unix_now)
}

/// How one path changed over the notice window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Updated,
    Removed,
}

impl ChangeKind {
    /// The notice keyword: `added`, `updated` or `removed`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Removed => "removed",
        }
    }
}

/// One path changed by a writer other than the lead since the last notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub path: String,
    pub kind: ChangeKind,
    pub writer: String,
    pub commit: i64,
}

/// What one notice block says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notices {
    pub compactions: u64,
    pub owner_turns: Option<i64>,
    pub changed: Vec<Changed>,
    pub expired: Vec<String>,
    /// Agreements flagged NEEDS-HUMAN whose notice was not shown yet.
    pub agreements: Vec<AgreementRow>,
    /// The owner turn number of a running onboarding, within `MAX_ONBOARDING_TURNS`.
    pub onboarding: Option<i64>,
}

/// What the lead was last told: the newest commit id, the expiry instant and the compaction count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watermarks {
    pub commit: i64,
    pub expiry: i64,
    pub compactions: u64,
}

impl Watermarks {
    /// The marks of a fresh pin: everything up to `head` and `now` is in the block.
    pub fn at_pin(head: i64, now: i64, compactions: u64) -> Self {
        Self {
            commit: head,
            expiry: now,
            compactions,
        }
    }

    /// After a re-pin: `commit = max(commit, head)`, `expiry = max(expiry, now)`; `compactions`
    /// is kept (the compaction still gets its line).
    pub fn repin(&mut self, head: i64, now: i64) {
        self.commit = self.commit.max(head);
        self.expiry = self.expiry.max(now);
    }
}

/// The notices since `marks`, advancing them. `owner_row` is the main owner turn's row (nudge
/// counted) or `None` for a steer. Newly flagged agreements are marked as noticed in the database
/// after every read succeeded, so each flag is told once, across restarts.
pub fn collect(
    store: &Store,
    marks: &mut Watermarks,
    compactions: u64,
    now: i64,
    owner_row: Option<i64>,
) -> Result<Notices, MemoryError> {
    let head = store.memory_head().map_err(history_error)?;
    let rows = store
        .memory_changes_since(marks.commit)
        .map_err(history_error)?;
    let committed = history::committed(store)?;
    let expired = expired_between(&committed, marks.expiry, now);
    let owner_turns = owner_row
        .map(|row| store.owner_turns_since_save(row))
        .transpose()
        .map_err(history_error)?;
    let onboarding = owner_row
        .map(|row| store.onboarding_turn(row))
        .transpose()
        .map_err(history_error)?
        .flatten()
        .filter(|n| (1..=MAX_ONBOARDING_TURNS).contains(n));
    let agreements = store.unnoticed_flags().map_err(history_error)?;
    let ids: Vec<i64> = agreements.iter().map(|a| a.id).collect();
    store.mark_flags_noticed(&ids).map_err(history_error)?;
    let notices = Notices {
        compactions: compactions.saturating_sub(marks.compactions),
        owner_turns,
        changed: changed(&rows),
        expired,
        agreements,
        onboarding,
    };
    *marks = Watermarks {
        commit: marks.commit.max(head),
        expiry: marks.expiry.max(now),
        compactions: marks.compactions.max(compactions),
    };
    Ok(notices)
}

fn history_error(e: StoreError) -> MemoryError {
    MemoryError {
        code: E_MEMORY_HISTORY,
        detail: e.code().to_string(),
    }
}

/// `<home>/memory/.refresh`: the explicit refresh request, `home` being the moraebot home.
pub fn refresh_file(home: &Path) -> PathBuf {
    home.join(MEMORY_DIR).join(REFRESH_FILE)
}

/// The non-lead changes in `rows` folded to one entry per path, sorted by path.
pub fn changed(rows: &[MemoryChangeRow]) -> Vec<Changed> {
    // Per path: the first row's `existed`, then the last row's `present`, writer and commit.
    let mut folded: BTreeMap<&str, (bool, &MemoryChangeRow)> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.writer != WRITER_LEAD) {
        folded
            .entry(row.path.as_str())
            .and_modify(|(_, last)| *last = row)
            .or_insert((row.existed, row));
    }
    folded
        .into_iter()
        .filter_map(|(path, (existed, last))| {
            let kind = match (existed, last.present) {
                (false, true) => ChangeKind::Added,
                (true, true) => ChangeKind::Updated,
                (true, false) => ChangeKind::Removed,
                (false, false) => return None,
            };
            Some(Changed {
                path: path.to_string(),
                kind,
                writer: last.writer.clone(),
                commit: last.commit_id,
            })
        })
        .collect()
}

/// The committed paths whose `expires` instant is in `(after, now]`, sorted by path.
pub fn expired_between(committed: &Committed, after: i64, now: i64) -> Vec<String> {
    committed
        .files
        .iter()
        .filter(|(_, text)| {
            frontmatter::parse(text).is_ok_and(|parsed| {
                parsed
                    .frontmatter
                    .get(EXPIRES_KEY)
                    .and_then(guard::expires_at)
                    .is_some_and(|at| after < at && at <= now)
            })
        })
        .map(|(path, _)| path.clone())
        .collect()
}

/// Whether `owner_turns` without a lead save calls for a nudge line.
pub fn nudge_due(owner_turns: i64) -> bool {
    owner_turns > 0 && owner_turns % NUDGE_EVERY == 0
}

/// The notice block for `notices`, or `None` when it would have no line.
pub fn block(notices: &Notices) -> Option<String> {
    let mut lines = Vec::new();
    if notices.compactions > 0 {
        lines.push(format!("compaction count={}", notices.compactions));
    }
    if let Some(n) = notices.owner_turns.filter(|n| nudge_due(*n)) {
        lines.push(format!("nudge owner_turns={n}"));
    }
    for c in notices
        .changed
        .iter()
        .filter(|c| SOUL_PATHS.contains(&c.path.as_str()))
    {
        lines.push(format!(
            "soul path={} writer={} commit={}",
            quoted(&c.path),
            c.writer,
            c.commit
        ));
    }
    for c in notices.changed.iter().take(MAX_NOTICE_PATHS) {
        lines.push(format!(
            "changed kind={} path={} writer={} commit={}",
            c.kind.name(),
            quoted(&c.path),
            c.writer,
            c.commit
        ));
    }
    if notices.changed.len() > MAX_NOTICE_PATHS {
        lines.push(format!(
            "changed more={}",
            notices.changed.len() - MAX_NOTICE_PATHS
        ));
    }
    for path in notices.expired.iter().take(MAX_NOTICE_PATHS) {
        lines.push(format!("expired path={}", quoted(path)));
    }
    if notices.expired.len() > MAX_NOTICE_PATHS {
        lines.push(format!(
            "expired more={}",
            notices.expired.len() - MAX_NOTICE_PATHS
        ));
    }
    for a in notices.agreements.iter().take(MAX_NOTICE_PATHS) {
        lines.push(format!(
            "agreement needs-human id={} broken={} text={}",
            a.id,
            a.broken,
            quoted(&a.text)
        ));
    }
    if notices.agreements.len() > MAX_NOTICE_PATHS {
        lines.push(format!(
            "agreement more={}",
            notices.agreements.len() - MAX_NOTICE_PATHS
        ));
    }
    if let Some(n) = notices.onboarding {
        lines.push(format!("onboarding turn={n}"));
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = format!("{NOTICE_BEGIN}\n");
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(NOTICE_END);
    out.push('\n');
    Some(out)
}

/// `input` with `block` right after its first line.
pub fn insert(input: &str, block: &str) -> String {
    match input.split_once('\n') {
        Some((tag, rest)) => format!("{tag}\n{block}{rest}"),
        None => format!("{input}\n{block}"),
    }
}

fn quoted(path: &str) -> String {
    serde_json::Value::from(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead;
    use crate::memory::home::MemoryHome;
    use crate::memory::{IDENTITY, NEEDS_HUMAN_AT, PERSONA, WRITER_HUMAN};
    use crate::store::{Admitted, MemoryCommit, OnboardingState};

    /// `notes/n.md` expires at unix second 150.
    const EXPIRING: &str = "---\ndescription: a note\nexpires: 1970-01-01T00:02:30Z\n---\nbody\n";

    fn store_fixture() -> (tempfile::TempDir, Store, MemoryHome) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("inbox.sqlite")).expect("store");
        let home = MemoryHome::new(dir.path());
        home.ensure_layout().expect("layout");
        (dir, store, home)
    }

    fn commit_by(
        store: &mut Store,
        home: &MemoryHome,
        writer: &str,
        changes: &[(&str, Option<&str>)],
    ) -> i64 {
        let commit = MemoryCommit {
            writer: writer.to_string(),
            reason: "r".to_string(),
            turn_row: None,
            conversation_key: None,
            claim_json: None,
        };
        let changes: Vec<_> = changes
            .iter()
            .map(|(path, body)| (path.to_string(), body.map(str::to_string)))
            .collect();
        history::record(store, home, &commit, &changes).expect("record")
    }

    fn owner_turn(store: &mut Store) -> i64 {
        let admitted = store
            .admit(
                "slack",
                "1300000000000000001",
                "slack:C111",
                "C111",
                "111",
                "hi",
            )
            .expect("admit");
        let Admitted::New(row) = admitted else {
            panic!("duplicate");
        };
        assert!(store.claim(row).expect("claim"));
        row
    }

    /// Like `owner_turn` with the message id numbered `n` (callers pass `n` from 2 upward).
    fn owner_turn_n(store: &mut Store, n: u32) -> i64 {
        let admitted = store
            .admit(
                "slack",
                &format!("13000000000000000{n:02}"),
                "slack:C111",
                "C111",
                "111",
                "hi",
            )
            .expect("admit");
        let Admitted::New(row) = admitted else {
            panic!("duplicate");
        };
        assert!(store.claim(row).expect("claim"));
        row
    }

    fn row(
        commit_id: i64,
        writer: &str,
        path: &str,
        existed: bool,
        present: bool,
    ) -> MemoryChangeRow {
        MemoryChangeRow {
            commit_id,
            writer: writer.to_string(),
            path: path.to_string(),
            existed,
            present,
        }
    }

    fn change(path: &str, kind: ChangeKind, commit: i64) -> Changed {
        Changed {
            path: path.to_string(),
            kind,
            writer: WRITER_HUMAN.to_string(),
            commit,
        }
    }

    #[test]
    fn refresh_file_is_under_the_memory_root() {
        let home = Path::new("qa-mbhome");
        assert_eq!(refresh_file(home), home.join("memory").join(".refresh"));
    }

    #[test]
    fn changed_drops_lead_and_folds_by_path() {
        let rows = [
            row(2, WRITER_LEAD, PERSONA, true, true),
            row(3, WRITER_HUMAN, "notes/a.md", true, true),
            row(4, WRITER_HUMAN, "notes/a.md", true, false),
            row(5, WRITER_HUMAN, "notes/b.md", false, true),
            row(6, WRITER_HUMAN, "notes/c.md", false, true),
            row(7, WRITER_HUMAN, "notes/c.md", true, false),
        ];
        assert_eq!(
            changed(&rows),
            vec![
                change("notes/a.md", ChangeKind::Removed, 4),
                change("notes/b.md", ChangeKind::Added, 5),
            ]
        );
    }

    #[test]
    fn expired_window_is_half_open() {
        let file = |expires: &str| format!("---\ndescription: d\nexpires: {expires}\n---\nbody\n");
        let mut committed = Committed::default();
        for (path, text) in [
            ("notes/at100.md", file("1970-01-01T00:01:40Z")),
            ("notes/at200.md", file("1970-01-01T00:03:20Z")),
            ("notes/at300.md", file("1970-01-01T00:05:00Z")),
            (
                "notes/never.md",
                "---\ndescription: d\n---\nbody\n".to_string(),
            ),
            (
                "notes/bad.md",
                "no frontmatter\nexpires: 1970-01-01T00:03:20Z\n".to_string(),
            ),
        ] {
            committed.files.insert(path.to_string(), text);
        }
        assert_eq!(
            expired_between(&committed, 100, 300),
            vec!["notes/at200.md".to_string(), "notes/at300.md".to_string()]
        );
    }

    #[test]
    fn nudge_every_ten() {
        for n in [0, 1, 9, 11] {
            assert!(!nudge_due(n), "{n}");
        }
        for n in [10, 20, 30] {
            assert!(nudge_due(n), "{n}");
        }
    }

    #[test]
    fn block_is_none_when_empty() {
        assert_eq!(block(&Notices::default()), None);
        let quiet = Notices {
            compactions: 0,
            owner_turns: Some(9),
            ..Notices::default()
        };
        assert_eq!(block(&quiet), None);
    }

    #[test]
    fn block_bytes_in_order() {
        let notices = Notices {
            compactions: 2,
            owner_turns: Some(10),
            changed: vec![
                change(IDENTITY, ChangeKind::Updated, 7),
                change(PERSONA, ChangeKind::Updated, 7),
            ],
            expired: vec!["notes/x y.md".to_string()],
            agreements: Vec::new(),
            onboarding: None,
        };
        let expected = concat!(
            "[moraebot memory-notice]\n",
            "compaction count=2\n",
            "nudge owner_turns=10\n",
            "soul path=\"system/identity.md\" writer=human commit=7\n",
            "soul path=\"system/persona.md\" writer=human commit=7\n",
            "changed kind=updated path=\"system/identity.md\" writer=human commit=7\n",
            "changed kind=updated path=\"system/persona.md\" writer=human commit=7\n",
            "expired path=\"notes/x y.md\"\n",
            "[moraebot memory-notice end]\n",
        );
        assert_eq!(block(&notices).as_deref(), Some(expected));
    }

    #[test]
    fn block_caps_paths_at_twenty() {
        let notices = Notices {
            changed: (0..23)
                .map(|i| change(&format!("notes/c{i:02}.md"), ChangeKind::Added, i + 1))
                .collect(),
            expired: (0..21).map(|i| format!("notes/e{i:02}.md")).collect(),
            ..Notices::default()
        };
        let text = block(&notices).expect("block");
        let lines: Vec<&str> = text.lines().collect();
        let count = |prefix: &str| lines.iter().filter(|l| l.starts_with(prefix)).count();
        assert_eq!(count("changed kind="), 20);
        assert_eq!(count("expired path="), 20);
        assert_eq!(count("changed more="), 1);
        assert_eq!(count("expired more="), 1);
        assert!(lines.contains(&"changed more=3"));
        assert!(lines.contains(&"expired more=1"));
        assert!(lines.contains(&"changed kind=added path=\"notes/c19.md\" writer=human commit=20"));
        assert!(!lines.iter().any(|l| l.contains("notes/c20.md")));
        assert!(!lines.iter().any(|l| l.contains("notes/e20.md")));
        assert_eq!(lines.first(), Some(&NOTICE_BEGIN));
        assert_eq!(lines.last(), Some(&NOTICE_END));
    }

    #[test]
    fn insert_goes_after_the_tag_line() {
        let b = "[moraebot memory-notice]\ncompaction count=1\n[moraebot memory-notice end]\n";
        let tag = "[moraebot conversation=k message=1]\n";
        let out = insert(&format!("{tag}hi"), b);
        assert_eq!(out, format!("{tag}{b}hi"));
        let parsed = lead::parse_tag(&out).expect("tag");
        assert_eq!(parsed.message, Some(1));
        assert_eq!(insert("tag", b), format!("tag\n{b}"));
    }

    #[test]
    fn collect_reports_once_and_advances() {
        let (_dir, mut store, home) = store_fixture();
        commit_by(&mut store, &home, WRITER_HUMAN, &[(PERSONA, Some("v1\n"))]);
        let head = store.memory_head().expect("head");
        let mut marks = Watermarks::at_pin(head, 100, 0);
        let human = commit_by(
            &mut store,
            &home,
            WRITER_HUMAN,
            &[(PERSONA, Some("v2\n")), ("notes/n.md", Some(EXPIRING))],
        );
        let row = owner_turn(&mut store);

        let first = collect(&store, &mut marks, 1, 200, Some(row)).expect("collect");
        assert_eq!(
            first,
            Notices {
                compactions: 1,
                owner_turns: Some(1),
                changed: vec![
                    change("notes/n.md", ChangeKind::Added, human),
                    change(PERSONA, ChangeKind::Updated, human),
                ],
                expired: vec!["notes/n.md".to_string()],
                agreements: Vec::new(),
                onboarding: None,
            }
        );
        assert_eq!(
            marks,
            Watermarks {
                commit: human,
                expiry: 200,
                compactions: 1
            }
        );

        let second = collect(&store, &mut marks, 1, 200, Some(row)).expect("collect");
        assert_eq!(
            second,
            Notices {
                owner_turns: Some(1),
                ..Notices::default()
            }
        );
        assert_eq!(
            marks,
            Watermarks {
                commit: human,
                expiry: 200,
                compactions: 1
            }
        );
    }

    #[test]
    fn collect_skips_lead_commits() {
        let (_dir, mut store, home) = store_fixture();
        let mut marks = Watermarks::at_pin(store.memory_head().expect("head"), 100, 0);
        let lead = commit_by(
            &mut store,
            &home,
            WRITER_LEAD,
            &[("notes/l.md", Some("l\n"))],
        );
        let notices = collect(&store, &mut marks, 0, 100, None).expect("collect");
        assert_eq!(notices, Notices::default());
        assert_eq!(marks.commit, lead);
    }

    #[test]
    fn repin_keeps_compactions() {
        let mut marks = Watermarks::at_pin(5, 100, 2);
        marks.repin(9, 300);
        let expected = Watermarks {
            commit: 9,
            expiry: 300,
            compactions: 2,
        };
        assert_eq!(marks, expected);
        marks.repin(3, 50);
        assert_eq!(marks, expected);
    }

    #[test]
    fn collect_error_leaves_marks() {
        let (_dir, mut store, home) = store_fixture();
        commit_by(
            &mut store,
            &home,
            WRITER_HUMAN,
            &[("notes/a.md", Some("a\n"))],
        );
        let mut marks = Watermarks::at_pin(0, 100, 0);
        store
            .exec_for_test("DROP TABLE memory_snapshots")
            .expect("drop");
        let err = collect(&store, &mut marks, 3, 500, None).expect_err("broken store");
        assert_eq!(err.code, E_MEMORY_HISTORY);
        assert_eq!(marks, Watermarks::at_pin(0, 100, 0));
    }

    fn flagged(id: i64, broken: i64, text: &str) -> AgreementRow {
        AgreementRow {
            id,
            scope: "project".to_string(),
            author: "agent".to_string(),
            text: text.to_string(),
            broken,
            needs_human: true,
        }
    }

    fn add_flagged(store: &mut Store, text: &str) -> i64 {
        let new = crate::store::NewAgreement {
            scope: "project",
            author: "agent",
            text,
            norm: text,
            turn_row: 1,
            reason: "why",
            claim_json: None,
        };
        let crate::store::AgreementAdded::Added(id) =
            store.add_agreement(&new, 64, 12288).expect("add")
        else {
            panic!("not added");
        };
        for turn in 10..10 + NEEDS_HUMAN_AT {
            store
                .break_agreement(id, turn, "slip", NEEDS_HUMAN_AT)
                .expect("break");
        }
        id
    }

    #[test]
    fn agreement_lines_follow_expired() {
        let notices = Notices {
            expired: vec!["notes/x.md".to_string()],
            agreements: vec![
                flagged(3, 3, "never force push"),
                flagged(5, 4, "ask \"first\""),
            ],
            ..Notices::default()
        };
        let expected = concat!(
            "[moraebot memory-notice]\n",
            "expired path=\"notes/x.md\"\n",
            "agreement needs-human id=3 broken=3 text=\"never force push\"\n",
            "agreement needs-human id=5 broken=4 text=\"ask \\\"first\\\"\"\n",
            "[moraebot memory-notice end]\n",
        );
        assert_eq!(block(&notices).as_deref(), Some(expected));
    }

    #[test]
    fn agreement_lines_cap_at_twenty() {
        let notices = Notices {
            agreements: (1..=21).map(|i| flagged(i, 3, "t")).collect(),
            ..Notices::default()
        };
        let text = block(&notices).expect("block");
        let lines: Vec<&str> = text.lines().collect();
        let count = |prefix: &str| lines.iter().filter(|l| l.starts_with(prefix)).count();
        assert_eq!(count("agreement needs-human "), 20);
        assert!(lines.contains(&"agreement more=1"));
        assert!(!lines.iter().any(|l| l.contains("id=21 ")));
    }

    #[test]
    fn block_with_only_agreements_is_some() {
        let notices = Notices {
            agreements: vec![flagged(1, 3, "t")],
            ..Notices::default()
        };
        assert!(block(&notices).is_some());
    }

    #[test]
    fn collect_tells_each_flag_once() {
        let (_dir, mut store, _home) = store_fixture();
        let id = add_flagged(&mut store, "never force push");
        let mut marks = Watermarks::at_pin(store.memory_head().expect("head"), 100, 0);
        let first = collect(&store, &mut marks, 0, 100, None).expect("collect");
        assert_eq!(first.agreements, vec![flagged(id, 3, "never force push")]);
        let second = collect(&store, &mut marks, 0, 100, None).expect("collect");
        assert!(second.agreements.is_empty());
        let fourth = store
            .break_agreement(id, 99, "slip", NEEDS_HUMAN_AT)
            .expect("break");
        assert!(matches!(
            fourth,
            crate::store::AgreementBreak::Counted {
                broken: 4,
                flagged: false,
                ..
            }
        ));
        let third = collect(&store, &mut marks, 0, 100, None).expect("collect");
        assert!(third.agreements.is_empty());
    }

    #[test]
    fn collect_error_marks_no_flag() {
        let (_dir, mut store, _home) = store_fixture();
        let id = add_flagged(&mut store, "never force push");
        let mut marks = Watermarks::at_pin(0, 100, 0);
        store
            .exec_for_test("DROP TABLE memory_snapshots")
            .expect("drop");
        let err = collect(&store, &mut marks, 0, 100, None).expect_err("broken store");
        assert_eq!(err.code, E_MEMORY_HISTORY);
        assert_eq!(marks, Watermarks::at_pin(0, 100, 0));
        assert_eq!(
            store.unnoticed_flags().expect("flags"),
            vec![flagged(id, 3, "never force push")]
        );
    }

    #[test]
    fn steer_collect_has_no_turn_count() {
        let (_dir, mut store, _home) = store_fixture();
        owner_turn(&mut store);
        let mut marks = Watermarks::at_pin(store.memory_head().expect("head"), 100, 0);
        let notices = collect(&store, &mut marks, 0, 100, None).expect("collect");
        assert_eq!(notices.owner_turns, None);
    }

    #[test]
    fn onboarding_line_comes_last() {
        let notices = Notices {
            changed: vec![change("notes/a.md", ChangeKind::Added, 4)],
            expired: vec!["notes/x.md".to_string()],
            agreements: vec![flagged(1, 3, "t")],
            onboarding: Some(2),
            ..Notices::default()
        };
        let text = block(&notices).expect("block");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            &lines[lines.len() - 2..],
            ["onboarding turn=2", NOTICE_END].as_slice()
        );
    }

    #[test]
    fn block_with_only_onboarding_is_some() {
        let notices = Notices {
            onboarding: Some(1),
            ..Default::default()
        };
        assert_eq!(
            block(&notices),
            Some(format!("{NOTICE_BEGIN}\nonboarding turn=1\n{NOTICE_END}\n"))
        );
    }

    #[test]
    fn collect_counts_onboarding_turns() {
        let (_dir, mut store, _home) = store_fixture();
        store.start_onboarding().expect("start");
        let a = owner_turn(&mut store);
        let b = owner_turn_n(&mut store, 2);
        let mut marks = Watermarks::at_pin(store.memory_head().expect("head"), 100, 0);
        let first = collect(&store, &mut marks, 0, 100, Some(a)).expect("collect");
        assert_eq!(first.onboarding, Some(1));
        let second = collect(&store, &mut marks, 0, 100, Some(b)).expect("collect");
        assert_eq!(second.onboarding, Some(2));
    }

    #[test]
    fn collect_stops_after_the_limit() {
        let (_dir, mut store, _home) = store_fixture();
        store.start_onboarding().expect("start");
        let rows: Vec<i64> = (2..=MAX_ONBOARDING_TURNS as u32 + 2)
            .map(|n| owner_turn_n(&mut store, n))
            .collect();
        let mut marks = Watermarks::at_pin(store.memory_head().expect("head"), 100, 0);
        let tenth = rows[MAX_ONBOARDING_TURNS as usize - 1];
        let eleventh = rows[MAX_ONBOARDING_TURNS as usize];
        let at_limit = collect(&store, &mut marks, 0, 100, Some(tenth)).expect("collect");
        assert_eq!(at_limit.onboarding, Some(MAX_ONBOARDING_TURNS));
        let past = collect(&store, &mut marks, 0, 100, Some(eleventh)).expect("collect");
        assert_eq!(past.onboarding, None);
    }

    #[test]
    fn steer_collect_has_no_onboarding() {
        let (_dir, mut store, _home) = store_fixture();
        store.start_onboarding().expect("start");
        let a = owner_turn(&mut store);
        let mut marks = Watermarks::at_pin(store.memory_head().expect("head"), 100, 0);
        let steer = collect(&store, &mut marks, 0, 100, None).expect("collect");
        assert_eq!(steer.onboarding, None);
        let turn = collect(&store, &mut marks, 0, 100, Some(a)).expect("collect");
        assert_eq!(turn.onboarding, Some(1));
    }

    #[test]
    fn collect_without_onboarding_is_none() {
        let (_dir, mut store, _home) = store_fixture();
        let a = owner_turn(&mut store);
        let mut marks = Watermarks::at_pin(store.memory_head().expect("head"), 100, 0);
        let none = collect(&store, &mut marks, 0, 100, Some(a)).expect("collect");
        assert_eq!(none.onboarding, None);
        store.start_onboarding().expect("start");
        assert!(
            store
                .finish_onboarding(OnboardingState::Done)
                .expect("finish")
        );
        let done = collect(&store, &mut marks, 0, 100, Some(a)).expect("collect");
        assert_eq!(done.onboarding, None);
    }

    #[test]
    fn system_clock_is_unix_now() {
        let before = guard::unix_now();
        let now = system_clock()();
        assert!((now - before).abs() <= 5, "{before} {now}");
    }
}
