//! The standing agreements ledger: agreements the owner and the lead keep, with broken counts.

use std::sync::Mutex;

use super::guard;
use super::history;
use super::home::MemoryError;
use super::provenance::{self, Claim, ClaimArgs, Kind, Source};
use super::write;
use super::{
    AGREEMENT_SCOPES, AUTHOR_AGENT, AUTHOR_HUMAN, BEGIN, E_MEMORY_AGREEMENT,
    E_MEMORY_AGREEMENT_EXISTS, E_MEMORY_AGREEMENT_FULL, E_MEMORY_AGREEMENT_REPEAT,
    E_MEMORY_AGREEMENT_UNKNOWN, E_MEMORY_ARGS, E_MEMORY_HISTORY, END, MAX_AGREEMENT_BYTES,
    MAX_AGREEMENT_CHARS, MAX_AGREEMENTS, MEMORY_AGREEMENT_BREAK_TOOL, MEMORY_AGREEMENT_RECORD_TOOL,
    NEEDS_HUMAN_AT, NOTICE_BEGIN, NOTICE_END,
};
use crate::omp::{TurnContext, lock};
use crate::store::{AgreementAdded, AgreementBreak, NewAgreement, Store, StoreError};

/// The comparison form of an agreement text: trimmed, inner whitespace runs folded to one
/// space, lowercased (`str::to_lowercase`).
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The arguments of one `memory_agreement_record` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub scope: String,
    pub text: String,
    pub reason: String,
    pub claim: ClaimArgs,
}

fn refused(code: &'static str, detail: impl Into<String>) -> MemoryError {
    MemoryError {
        code,
        detail: detail.into(),
    }
}

fn history_err(e: StoreError) -> MemoryError {
    refused(E_MEMORY_HISTORY, e.code())
}

/// The agreement text as stored (trimmed) when it is one line of 1..=MAX_AGREEMENT_CHARS
/// characters with no NUL, no block sentinel and nothing secret-like.
pub fn check_text(text: &str) -> Result<String, MemoryError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(refused(E_MEMORY_ARGS, "text is empty"));
    }
    if text.contains(['\n', '\r', '\0']) {
        return Err(refused(E_MEMORY_ARGS, "text must be one line without NUL"));
    }
    if text.chars().count() > MAX_AGREEMENT_CHARS {
        return Err(refused(
            E_MEMORY_ARGS,
            format!("text is longer than {MAX_AGREEMENT_CHARS} characters"),
        ));
    }
    if [BEGIN, END, NOTICE_BEGIN, NOTICE_END]
        .iter()
        .any(|sentinel| text.contains(sentinel))
    {
        return Err(refused(
            E_MEMORY_ARGS,
            "text contains a memory block sentinel",
        ));
    }
    guard::refuse_secret("text", text)?;
    Ok(text.to_string())
}

/// Records one agreement for the running owner turn; the result JSON or a coded refusal.
pub fn record(
    store: &Mutex<Store>,
    turn: Option<&TurnContext>,
    req: &Record,
) -> Result<String, MemoryError> {
    let tool = MEMORY_AGREEMENT_RECORD_TOOL;
    let result = run_record(store, turn, req);
    match &result {
        Ok((id, author, _)) => {
            tracing::info!(tool, agreement = id, author, "memory agreement recorded");
        }
        Err(e) => tracing::info!(tool, code = e.code, "memory agreement refused"),
    }
    result.map(|(_, _, json)| json)
}

fn run_record(
    store: &Mutex<Store>,
    turn: Option<&TurnContext>,
    req: &Record,
) -> Result<(i64, &'static str, String), MemoryError> {
    history::check_reason(&req.reason)?;
    if let Some(quote) = &req.claim.user_quote {
        guard::refuse_secret("user_quote", quote)?;
    }

    let mut store = lock(store);
    let turn = write::owner_turn(&store, turn)?;

    let Some(scope) = AGREEMENT_SCOPES.iter().copied().find(|s| *s == req.scope) else {
        return Err(refused(E_MEMORY_ARGS, "scope must be global or project"));
    };
    let text = check_text(&req.text)?;

    let (author, claim_json) = match req.claim.source {
        None => (AUTHOR_AGENT, None),
        Some(source @ (Source::Agent | Source::Inferred)) => {
            (AUTHOR_AGENT, Some(Claim::plain(source).to_json()))
        }
        Some(Source::User) => {
            let claim = provenance::verify(&store, turn, &req.claim)?;
            if claim.kind != Kind::User {
                return Err(refused(E_MEMORY_AGREEMENT, claim.code.unwrap_or("")));
            }
            (AUTHOR_HUMAN, Some(claim.to_json()))
        }
    };

    let norm = normalize(&text);
    let new = NewAgreement {
        scope,
        author,
        text: &text,
        norm: &norm,
        turn_row: turn.row_id,
        reason: &req.reason,
        claim_json: claim_json.as_deref(),
    };
    let id = match store
        .add_agreement(&new, MAX_AGREEMENTS, MAX_AGREEMENT_BYTES)
        .map_err(history_err)?
    {
        AgreementAdded::Added(id) => id,
        AgreementAdded::Exists(id) => {
            return Err(refused(E_MEMORY_AGREEMENT_EXISTS, format!("#{id}")));
        }
        AgreementAdded::Full => {
            return Err(refused(
                E_MEMORY_AGREEMENT_FULL,
                format!(
                    "the ledger holds {MAX_AGREEMENTS} agreements or {MAX_AGREEMENT_BYTES} bytes"
                ),
            ));
        }
    };
    let json = serde_json::json!({
        "agreement": id,
        "scope": scope,
        "author": author,
        "broken": 0,
        "needs_human": false,
    })
    .to_string();
    Ok((id, author, json))
}

/// Counts one break of agreement `id` for the running owner turn; the result JSON or a coded refusal.
pub fn break_one(
    store: &Mutex<Store>,
    turn: Option<&TurnContext>,
    id: i64,
    reason: &str,
) -> Result<String, MemoryError> {
    let tool = MEMORY_AGREEMENT_BREAK_TOOL;
    let result = run_break(store, turn, id, reason);
    match &result {
        Ok((broken, flagged, _)) => {
            tracing::info!(
                tool,
                agreement = id,
                broken,
                flagged,
                "memory agreement broken"
            );
        }
        Err(e) => tracing::info!(tool, code = e.code, "memory agreement refused"),
    }
    result.map(|(_, _, json)| json)
}

fn run_break(
    store: &Mutex<Store>,
    turn: Option<&TurnContext>,
    id: i64,
    reason: &str,
) -> Result<(i64, bool, String), MemoryError> {
    history::check_reason(reason)?;

    let mut store = lock(store);
    let turn = write::owner_turn(&store, turn)?;

    match store
        .break_agreement(id, turn.row_id, reason, NEEDS_HUMAN_AT)
        .map_err(history_err)?
    {
        AgreementBreak::Unknown => Err(refused(E_MEMORY_AGREEMENT_UNKNOWN, format!("#{id}"))),
        AgreementBreak::Repeat => Err(refused(E_MEMORY_AGREEMENT_REPEAT, format!("#{id}"))),
        AgreementBreak::Counted {
            broken,
            needs_human,
            flagged,
        } => {
            let json = serde_json::json!({
                "agreement": id,
                "broken": broken,
                "needs_human": needs_human,
                "flagged": flagged,
            })
            .to_string();
            Ok((broken, flagged, json))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::E_MEMORY_ORIGIN;
    use crate::memory::E_MEMORY_QUOTE_MISSING;
    use crate::memory::E_MEMORY_SECRET;
    use crate::store::{Admitted, LaneStart, LaneState};
    use serde_json::{Value, json};

    const REASON: &str = "the owner set a lasting rule";

    struct Fx {
        _dir: tempfile::TempDir,
        store: Mutex<Store>,
    }

    impl Fx {
        fn owner_row(&self, text: &str) -> i64 {
            let mut store = lock(&self.store);
            let message_id = format!("m{}", store.count().expect("count") + 1);
            match store
                .admit("test", &message_id, "k1", "chan1", "111", text)
                .expect("admit")
            {
                Admitted::New(id) => id,
                Admitted::Duplicate => panic!("duplicate row"),
            }
        }

        fn owner_turn(&self, text: &str) -> TurnContext {
            let row = self.owner_row(text);
            assert!(lock(&self.store).claim(row).expect("claim"));
            turn(row)
        }

        /// An owner turn that approves a proposal: an owner row answered with a reply, then
        /// the approval row `yes`. Returns the turn on the approval row and its id.
        fn approved_turn(&self) -> (TurnContext, i64) {
            let proposal = self.owner_row("please remember");
            {
                let mut store = lock(&self.store);
                assert!(store.claim(proposal).expect("claim"));
                store
                    .finish_group(proposal, "proposal-text", None, None)
                    .expect("finish");
            }
            let approval = self.owner_row("yes");
            (turn(approval), approval)
        }

        fn lane_turn(&self) -> TurnContext {
            let mut store = lock(&self.store);
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
            let row = store.count().expect("count");
            assert_eq!(store.get(row).expect("get").expect("row").origin, "lane");
            turn(row)
        }

        fn agreements(&self) -> Vec<crate::store::AgreementRow> {
            lock(&self.store).agreements().expect("agreements")
        }
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("inbox.sqlite")).expect("store");
        Fx {
            _dir: dir,
            store: Mutex::new(store),
        }
    }

    fn turn(row_id: i64) -> TurnContext {
        TurnContext {
            row_id,
            conversation_key: "k1".to_string(),
        }
    }

    fn req(scope: &str, text: &str, claim: ClaimArgs) -> Record {
        Record {
            scope: scope.to_string(),
            text: text.to_string(),
            reason: REASON.to_string(),
            claim,
        }
    }

    fn plain(scope: &str, text: &str) -> Record {
        req(scope, text, ClaimArgs::default())
    }

    fn user(quote: Option<&str>, message: Option<i64>) -> ClaimArgs {
        ClaimArgs {
            source: Some(Source::User),
            user_quote: quote.map(str::to_string),
            quote_message: message,
        }
    }

    fn json_of(text: &str) -> Value {
        serde_json::from_str(text).expect("result json")
    }

    /// A secret-like value built at run time: `parts` joined, then `n` copies of `fill`, so no
    /// source line holds a whole token.
    fn fake(parts: &[&str], fill: char, n: usize) -> String {
        let mut out = parts.concat();
        out.extend(std::iter::repeat_n(fill, n));
        out
    }

    #[test]
    fn normalize_folds_case_and_spaces() {
        assert_eq!(normalize("  Never  Push\tTO main "), "never push to main");
        assert_eq!(normalize("커밋 전에  묻기"), "커밋 전에 묻기");
    }

    #[test]
    fn record_by_agent_without_source() {
        let fx = fx();
        let t = fx.owner_turn("from now on never force push");
        let out = record(&fx.store, Some(&t), &plain("global", "  Never force push ")).unwrap();
        assert_eq!(
            json_of(&out),
            json!({"agreement":1,"scope":"global","author":"agent","broken":0,"needs_human":false})
        );
        let rows = fx.agreements();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[0].scope, "global");
        assert_eq!(rows[0].author, AUTHOR_AGENT);
        assert_eq!(rows[0].text, "Never force push");
        assert_eq!((rows[0].broken, rows[0].needs_human), (0, false));
    }

    #[test]
    fn record_with_verified_quote_is_human() {
        let fx = fx();
        let first = fx.owner_turn("ask before a release");
        record(
            &fx.store,
            Some(&first),
            &plain("project", "ask before a release"),
        )
        .unwrap();
        let (t, row) = fx.approved_turn();
        let out = record(
            &fx.store,
            Some(&t),
            &req("global", "never force push", user(Some("yes"), Some(row))),
        )
        .unwrap();
        let out = json_of(&out);
        assert_eq!(out["agreement"], 2);
        assert_eq!(out["author"], AUTHOR_HUMAN);
        let rows = fx.agreements();
        assert_eq!(
            rows.iter()
                .map(|r| (r.id, r.author.as_str()))
                .collect::<Vec<_>>(),
            [(2, AUTHOR_HUMAN), (1, AUTHOR_AGENT)]
        );
    }

    #[test]
    fn unverified_user_source_is_refused() {
        let fx = fx();
        let t = fx.owner_turn("maybe later");
        let not_approval = record(
            &fx.store,
            Some(&t),
            &req(
                "global",
                "never force push",
                user(Some("maybe later"), Some(t.row_id)),
            ),
        )
        .unwrap_err();
        assert_eq!(not_approval.code, E_MEMORY_AGREEMENT);
        assert!(
            not_approval.detail.starts_with("E_MEMORY_QUOTE_"),
            "{not_approval}"
        );

        let missing = record(
            &fx.store,
            Some(&t),
            &req("global", "never force push", user(None, None)),
        )
        .unwrap_err();
        assert_eq!(missing.code, E_MEMORY_AGREEMENT);
        assert_eq!(missing.detail, E_MEMORY_QUOTE_MISSING);
        assert!(fx.agreements().is_empty());
    }

    #[test]
    fn record_text_rules() {
        let fx = fx();
        let t = fx.owner_turn("rules");
        let too_long = "가".repeat(MAX_AGREEMENT_CHARS + 1);
        let with_begin = format!("keep {BEGIN} out");
        let with_notice = format!("keep {NOTICE_BEGIN} out");
        for text in [
            "",
            "   ",
            "a\nb",
            too_long.as_str(),
            with_begin.as_str(),
            with_notice.as_str(),
        ] {
            let err = record(&fx.store, Some(&t), &plain("global", text)).unwrap_err();
            assert_eq!(err.code, E_MEMORY_ARGS, "{text:?}");
        }

        let secret = fake(&["gh", "p_"], 'A', 36);
        let err = record(
            &fx.store,
            Some(&t),
            &plain("global", &format!("use {secret} for pushes")),
        )
        .unwrap_err();
        assert_eq!(err.code, E_MEMORY_SECRET);
        assert!(!err.detail.contains(&secret));
        assert!(!err.to_string().contains(&secret));
        assert!(fx.agreements().is_empty());

        let longest = "가".repeat(MAX_AGREEMENT_CHARS);
        let out = record(&fx.store, Some(&t), &plain("global", &longest)).unwrap();
        assert_eq!(json_of(&out)["agreement"], 1);
        assert_eq!(fx.agreements()[0].text, longest);
    }

    #[test]
    fn record_refusals_change_nothing() {
        let fx = fx();
        let t = fx.owner_turn("rules");
        record(&fx.store, Some(&t), &plain("global", "rule 1")).unwrap();
        let before = fx.agreements();

        let refusals = [
            (None, plain("global", "rule x"), E_MEMORY_ORIGIN, None),
            (
                Some(fx.lane_turn()),
                plain("global", "rule x"),
                E_MEMORY_ORIGIN,
                None,
            ),
            (
                Some(t.clone()),
                plain("team", "rule x"),
                E_MEMORY_ARGS,
                None,
            ),
            (
                Some(t.clone()),
                plain("project", "  RULE   1 "),
                E_MEMORY_AGREEMENT_EXISTS,
                Some("#1"),
            ),
            (
                Some(t.clone()),
                Record {
                    reason: "one\ntwo".to_string(),
                    ..plain("global", "rule x")
                },
                E_MEMORY_ARGS,
                None,
            ),
        ];
        for (turn, req, code, detail) in refusals {
            let err = record(&fx.store, turn.as_ref(), &req).unwrap_err();
            assert_eq!(err.code, code, "{req:?}");
            if let Some(detail) = detail {
                assert_eq!(err.detail, detail);
            }
            assert_eq!(fx.agreements(), before, "{req:?}");
        }

        for n in 2..=MAX_AGREEMENTS {
            record(&fx.store, Some(&t), &plain("global", &format!("rule {n}"))).unwrap();
        }
        let full = fx.agreements();
        assert_eq!(full.len(), MAX_AGREEMENTS);
        let err = record(&fx.store, Some(&t), &plain("global", "rule 65")).unwrap_err();
        assert_eq!(err.code, E_MEMORY_AGREEMENT_FULL);
        assert_eq!(fx.agreements(), full);
    }

    #[test]
    fn break_counts_and_flags_through_the_handler() {
        let fx = fx();
        let t0 = fx.owner_turn("rules");
        record(&fx.store, Some(&t0), &plain("global", "never force push")).unwrap();
        lock(&fx.store)
            .finish_group(t0.row_id, "ok", None, None)
            .expect("finish");

        let mut turns = Vec::new();
        for text in ["r1", "r2", "r3"] {
            let t = fx.owner_turn(text);
            turns.push(t.clone());
            let out = json_of(&break_one(&fx.store, Some(&t), 1, REASON).unwrap());
            let n = turns.len() as i64;
            assert_eq!(
                out,
                json!({"agreement":1,"broken":n,"needs_human":n >= 3,"flagged":n == 3})
            );
            if n < 3 {
                lock(&fx.store)
                    .finish_group(t.row_id, "ok", None, None)
                    .expect("finish");
            }
        }
        assert!(turns[0].row_id < turns[1].row_id && turns[1].row_id < turns[2].row_id);

        let r3 = &turns[2];
        let repeat = break_one(&fx.store, Some(r3), 1, REASON).unwrap_err();
        assert_eq!(repeat.code, E_MEMORY_AGREEMENT_REPEAT);
        assert_eq!(repeat.detail, "#1");
        let unknown = break_one(&fx.store, Some(r3), 7, REASON).unwrap_err();
        assert_eq!(unknown.code, E_MEMORY_AGREEMENT_UNKNOWN);
        assert_eq!(unknown.detail, "#7");
        let no_turn = break_one(&fx.store, None, 1, REASON).unwrap_err();
        assert_eq!(no_turn.code, E_MEMORY_ORIGIN);

        let rows = fx.agreements();
        assert_eq!((rows[0].broken, rows[0].needs_human), (3, true));
    }
}
