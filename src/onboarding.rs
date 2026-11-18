//! The first meeting with the owner: the request file, the mark on the lead's input and the lead's
//! finish tool.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use omp_rpc::{HostTool, HostToolError, ToolLoadMode};
use serde_json::{Map, Value};

use crate::omp::{self, TurnContext};
use crate::persona;
use crate::store::{OnboardingState, Store, StoreError};

/// The file under the moraebot home that asks for a first meeting.
pub const REQUEST_FILE: &str = "onboarding.request";
/// The lead's host tool that ends the first meeting.
pub const FINISH_TOOL: &str = "onboarding_finish";
/// How a first meeting can end.
pub const OUTCOMES: [&str; 2] = ["done", "skipped"];
/// The most owner turns that carry the onboarding mark.
pub const MAX_TURNS: i64 = 10;
/// The request file could not be written or removed.
pub const E_ONBOARDING_REQUEST: &str = "E_ONBOARDING_REQUEST";
/// The tool arguments are malformed.
pub const E_ONBOARDING_ARGS: &str = "E_ONBOARDING_ARGS";
/// The tool was called while not answering an owner message.
pub const E_ONBOARDING_ORIGIN: &str = "E_ONBOARDING_ORIGIN";
/// No onboarding is running.
pub const E_ONBOARDING_STATE: &str = "E_ONBOARDING_STATE";

/// The keys of the finish tool's arguments, all required.
const FINISH_KEYS: [&str; 4] = ["outcome", "name", "tone", "owner"];
/// The finish tool's description for the lead.
const FINISH_DESCRIPTION: &str = "End the first meeting with the owner. outcome done saves name, tone and owner (short single lines, no keys or passwords) into persona.md, which applies the next time moraebot starts; outcome skipped saves nothing (pass empty strings). Works only while you answer an owner message.";

/// A coded refusal; `detail` never holds field text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingError {
    pub code: &'static str,
    pub detail: String,
}

/// `<home>/onboarding.request`.
pub fn request_file(home: &Path) -> PathBuf {
    home.join(REQUEST_FILE)
}

/// Creates or truncates the request file, mode 0600. `E_ONBOARDING_REQUEST` with the io error
/// kind as detail when it cannot be written.
pub fn request(home: &Path) -> Result<(), OnboardingError> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(request_file(home))
        .map(|_| ())
        .map_err(|e| OnboardingError {
            code: E_ONBOARDING_REQUEST,
            detail: format!("{:?}", e.kind()),
        })
}

/// `[moraebot onboarding turn=<turn>]`.
pub fn notice_line(turn: i64) -> String {
    format!("[moraebot onboarding turn={turn}]")
}

/// `input` with `notice_line(turn)` inserted as its second line (after the first `\n`; appended
/// after a `\n` when `input` has no newline).
pub fn mark_input(input: &str, turn: i64) -> String {
    let line = notice_line(turn);
    match input.split_once('\n') {
        Some((first, rest)) => format!("{first}\n{line}\n{rest}"),
        None => format!("{input}\n{line}"),
    }
}

fn refusal(code: &'static str, detail: impl Into<String>) -> OnboardingError {
    OnboardingError {
        code,
        detail: detail.into(),
    }
}

fn store_refusal(e: StoreError) -> OnboardingError {
    refusal(e.code(), e.to_string())
}

/// Ends the running onboarding for the running owner turn. `done` saves `persona.md` first.
/// The result JSON, or a coded refusal.
pub fn finish(
    store: &Mutex<Store>,
    home: &Path,
    turn: Option<&TurnContext>,
    args: &Map<String, Value>,
) -> Result<String, OnboardingError> {
    let result = end_onboarding(store, home, turn, args);
    match &result {
        Ok((outcome, _)) => tracing::info!(tool = FINISH_TOOL, outcome, "onboarding finished"),
        Err(e) => tracing::info!(
            tool = FINISH_TOOL,
            code = e.code,
            "onboarding finish refused"
        ),
    }
    result.map(|(_, json)| json)
}

/// The steps of [`finish`]: the outcome and the result JSON, or a coded refusal.
fn end_onboarding(
    store: &Mutex<Store>,
    home: &Path,
    turn: Option<&TurnContext>,
    args: &Map<String, Value>,
) -> Result<(&'static str, String), OnboardingError> {
    if let Some(key) = args.keys().find(|k| !FINISH_KEYS.contains(&k.as_str())) {
        return Err(refusal(E_ONBOARDING_ARGS, format!("unknown key {key}")));
    }
    let field = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| refusal(E_ONBOARDING_ARGS, format!("{key} must be a string")))
    };
    let (outcome, name, tone, owner) = (
        field("outcome")?,
        field("name")?,
        field("tone")?,
        field("owner")?,
    );
    let outcome = OUTCOMES
        .into_iter()
        .find(|o| *o == outcome)
        .ok_or_else(|| refusal(E_ONBOARDING_ARGS, "outcome must be done or skipped"))?;

    let not_owner = || refusal(E_ONBOARDING_ORIGIN, "not answering an owner message");
    let turn = turn.ok_or_else(not_owner)?;
    let store = omp::lock(store);
    let row = store.get(turn.row_id).map_err(store_refusal)?;
    if !row.is_some_and(|row| row.origin == "owner") {
        return Err(not_owner());
    }
    let not_running = || refusal(E_ONBOARDING_STATE, "no onboarding is running");
    let running = store.onboarding().map_err(store_refusal)?;
    if !running.is_some_and(|row| row.state == OnboardingState::Active) {
        return Err(not_running());
    }

    let done = outcome == "done";
    if done {
        let persona_refusal = |e: persona::PersonaError| refusal(e.code, e.detail);
        let text = persona::render(name, tone, owner).map_err(persona_refusal)?;
        persona::save(home, &text).map_err(persona_refusal)?;
    }
    let end = if done {
        OnboardingState::Done
    } else {
        OnboardingState::Skipped
    };
    if !store.finish_onboarding(end).map_err(store_refusal)? {
        return Err(not_running());
    }
    let result = if done {
        serde_json::json!({"onboarding": "done", "persona": "saved", "applies": "next_start"})
    } else {
        serde_json::json!({"onboarding": "skipped", "persona": "unchanged"})
    };
    Ok((outcome, result.to_string()))
}

/// The `onboarding_finish` host tool (Essential), reading the turn from `turn`.
pub fn host_tools(
    store: Arc<Mutex<Store>>,
    home: PathBuf,
    turn: Arc<Mutex<Option<TurnContext>>>,
) -> Vec<HostTool> {
    let parameters: Map<String, Value> = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "outcome": {"type": "string", "enum": ["done", "skipped"]},
            "name": {"type": "string", "description": "What the owner wants to call you; one line."},
            "tone": {"type": "string", "description": "How you should talk; one line."},
            "owner": {"type": "string", "description": "A few basics about the owner in their words; one line."}
        },
        "required": ["outcome", "name", "tone", "owner"],
        "additionalProperties": false
    }))
    .expect("onboarding_finish schema is a JSON object");
    let tool = HostTool::new(
        FINISH_TOOL,
        FINISH_DESCRIPTION,
        parameters,
        move |arguments, _| {
            let context = omp::lock(&turn).clone();
            finish(&store, &home, context.as_ref(), &arguments)
                .map(Into::into)
                .map_err(|e| -> HostToolError { format!("{}: {}", e.code, e.detail).into() })
        },
    )
    .load_mode(ToolLoadMode::Essential);
    vec![tool]
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::lead;
    use crate::store::{Admitted, LaneState};

    /// A store in `<tmp>/qa-mbhome` with owner row 1 claimed, and the turn of row 1.
    fn fixture() -> (tempfile::TempDir, PathBuf, Mutex<Store>, TurnContext) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("qa-mbhome");
        std::fs::create_dir(&home).unwrap();
        let mut store = Store::open(&home.join("inbox.sqlite")).unwrap();
        assert_eq!(
            store
                .admit("discord", "m1", "p:th:111", "222", "111", "hi")
                .unwrap(),
            Admitted::New(1)
        );
        assert!(store.claim(1).unwrap());
        let turn = TurnContext {
            row_id: 1,
            conversation_key: "p:th:111".into(),
        };
        (dir, home, Mutex::new(store), turn)
    }

    fn args(outcome: &str, name: &str, tone: &str, owner: &str) -> Map<String, Value> {
        serde_json::from_value(
            serde_json::json!({"outcome": outcome, "name": name, "tone": tone, "owner": owner}),
        )
        .unwrap()
    }

    fn state(store: &Mutex<Store>) -> Option<OnboardingState> {
        store.lock().unwrap().onboarding().unwrap().map(|r| r.state)
    }

    fn start(store: &Mutex<Store>) {
        store.lock().unwrap().start_onboarding().unwrap();
    }

    fn code(result: Result<String, OnboardingError>) -> &'static str {
        result.expect_err("refused").code
    }

    #[test]
    fn finish_done_saves_then_ends() {
        let (_dir, home, store, turn) = fixture();
        start(&store);
        let out = finish(
            &store,
            &home,
            Some(&turn),
            &args(
                "done",
                "Sora",
                "short and warm",
                "Owner 111 likes short replies",
            ),
        )
        .expect("done");
        let out: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            out,
            serde_json::json!({"onboarding": "done", "persona": "saved", "applies": "next_start"})
        );
        assert_eq!(
            std::fs::read_to_string(persona::path(&home)).unwrap(),
            persona::render("Sora", "short and warm", "Owner 111 likes short replies").unwrap()
        );
        assert_eq!(state(&store), Some(OnboardingState::Done));
    }

    #[test]
    fn finish_skipped_leaves_no_file() {
        let (_dir, home, store, turn) = fixture();
        start(&store);
        let out = finish(&store, &home, Some(&turn), &args("skipped", "", "", "")).unwrap();
        let out: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            out,
            serde_json::json!({"onboarding": "skipped", "persona": "unchanged"})
        );
        assert!(!persona::path(&home).exists());
        assert_eq!(state(&store), Some(OnboardingState::Skipped));
    }

    #[test]
    fn finish_needs_an_owner_turn() {
        let (_dir, home, store, _turn) = fixture();
        start(&store);
        let done = args("done", "Sora", "", "");
        assert_eq!(
            code(finish(&store, &home, None, &done)),
            E_ONBOARDING_ORIGIN
        );
        {
            let mut s = store.lock().unwrap();
            s.start_lane("call-2", "p:th:111", "t", None, 3).unwrap();
            assert!(s.finish_lane(1, 1, "r", LaneState::Idle, None).unwrap());
            assert_eq!(s.get(2).unwrap().expect("lane row").origin, "lane");
        }
        let lane_turn = TurnContext {
            row_id: 2,
            conversation_key: "p:th:111".into(),
        };
        assert_eq!(
            code(finish(&store, &home, Some(&lane_turn), &done)),
            E_ONBOARDING_ORIGIN
        );
        assert_eq!(state(&store), Some(OnboardingState::Active));
        assert!(!persona::path(&home).exists());
    }

    #[test]
    fn finish_without_onboarding_is_refused() {
        let (_dir, home, store, turn) = fixture();
        let done = args("done", "Sora", "", "");
        assert_eq!(
            code(finish(&store, &home, Some(&turn), &done)),
            E_ONBOARDING_STATE
        );
        assert_eq!(state(&store), None);
        assert!(!persona::path(&home).exists());
    }

    #[test]
    fn finish_twice_is_refused() {
        let (_dir, home, store, turn) = fixture();
        start(&store);
        let done = args("done", "Sora", "", "");
        finish(&store, &home, Some(&turn), &done).expect("first finish");
        assert_eq!(
            code(finish(&store, &home, Some(&turn), &done)),
            E_ONBOARDING_STATE
        );
    }

    #[test]
    fn finish_args_are_checked() {
        let (_dir, home, store, turn) = fixture();
        start(&store);
        let mut extra = args("done", "Sora", "", "");
        extra.insert("x".into(), Value::from("y"));
        let mut missing = args("done", "Sora", "", "");
        missing.remove("tone");
        let mut number = args("done", "", "", "");
        number.insert("name".into(), Value::from(1));
        for bad in [args("later", "Sora", "", ""), extra, missing, number] {
            assert_eq!(
                code(finish(&store, &home, Some(&turn), &bad)),
                E_ONBOARDING_ARGS,
                "{bad:?}"
            );
        }
        assert_eq!(state(&store), Some(OnboardingState::Active));
    }

    #[test]
    fn finish_refuses_a_bad_persona() {
        let (_dir, home, store, turn) = fixture();
        start(&store);
        assert_eq!(
            code(finish(
                &store,
                &home,
                Some(&turn),
                &args("done", "", "t", "o")
            )),
            persona::E_PERSONA_FIELD
        );
        let owner = ["xo", "xb-"].concat() + &"1".repeat(10);
        let err = finish(
            &store,
            &home,
            Some(&turn),
            &args("done", "Sora", "", &owner),
        )
        .expect_err("secret refused");
        assert_eq!(err.code, persona::E_PERSONA_SECRET);
        assert!(!err.detail.contains(&owner));
        assert_eq!(state(&store), Some(OnboardingState::Active));
        assert!(!persona::path(&home).exists());
    }

    #[test]
    fn request_file_is_under_the_home() {
        let home = Path::new("qa-mbhome");
        assert_eq!(request_file(home), home.join("onboarding.request"));
    }

    #[test]
    fn request_creates_a_private_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("qa-mbhome");
        std::fs::create_dir(&home).unwrap();
        request(&home).expect("request");
        let meta = std::fs::metadata(request_file(&home)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.len(), 0);
        request(&home).expect("second request");
    }

    #[test]
    fn request_failure_is_coded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("qa-mbhome");
        std::fs::write(&home, b"x").unwrap();
        let err = request(&home).expect_err("home is a file");
        assert_eq!(err.code, E_ONBOARDING_REQUEST);
    }

    #[test]
    fn mark_input_puts_the_line_second() {
        let marked = mark_input("[moraebot conversation=k message=3]\nhello", 2);
        assert_eq!(
            marked,
            "[moraebot conversation=k message=3]\n[moraebot onboarding turn=2]\nhello"
        );
        assert_eq!(lead::parse_tag(&marked).expect("tag").message, Some(3));
    }

    #[test]
    fn mark_input_without_newline() {
        assert_eq!(mark_input("t", 1), "t\n[moraebot onboarding turn=1]");
    }
}
