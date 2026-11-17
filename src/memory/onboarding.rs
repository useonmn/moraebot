//! The first meeting with the owner: the request file, the state in the inbox database and the
//! lead's finish tool.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::history;
use super::home::{MemoryError, MemoryHome};
use super::write;
use super::{
    E_MEMORY_ARGS, E_MEMORY_HISTORY, E_MEMORY_WRITE, E_ONBOARDING_REQUEST, E_ONBOARDING_STATE,
    ONBOARDING_FINISH_TOOL, ONBOARDING_REQUEST_FILE, REFRESH_FILE,
};
use crate::omp::{TurnContext, lock};
use crate::store::{OnboardingState, Store};

/// `<home>/onboarding.request`, `home` being the moraebot home.
pub fn request_file(home: &Path) -> PathBuf {
    home.join(ONBOARDING_REQUEST_FILE)
}

/// Creates (or truncates) the request file, mode 0600. `E_ONBOARDING_REQUEST` with the
/// io error kind as detail when it cannot be written.
pub fn request(home: &Path) -> Result<(), MemoryError> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(request_file(home))
        .map(|_| ())
        .map_err(|e| MemoryError {
            code: E_ONBOARDING_REQUEST,
            detail: format!("{:?}", e.kind()),
        })
}

/// Ends the running onboarding for the running owner turn: `outcome` is `done` or `skipped`.
/// `done` also asks for a re-pin through the memory refresh file. The result JSON or a coded refusal.
pub fn finish(
    store: &Mutex<Store>,
    home: &MemoryHome,
    turn: Option<&TurnContext>,
    outcome: &str,
    reason: &str,
) -> Result<String, MemoryError> {
    let result = run_finish(store, home, turn, outcome, reason);
    match &result {
        Ok(_) => tracing::info!(
            tool = ONBOARDING_FINISH_TOOL,
            outcome,
            "onboarding finished"
        ),
        Err(e) => tracing::info!(
            tool = ONBOARDING_FINISH_TOOL,
            code = e.code,
            "onboarding finish refused"
        ),
    }
    result
}

fn run_finish(
    store: &Mutex<Store>,
    home: &MemoryHome,
    turn: Option<&TurnContext>,
    outcome: &str,
    reason: &str,
) -> Result<String, MemoryError> {
    history::check_reason(reason)?;
    let state = match outcome {
        "done" => OnboardingState::Done,
        "skipped" => OnboardingState::Skipped,
        _ => {
            return Err(MemoryError {
                code: E_MEMORY_ARGS,
                detail: "outcome must be done or skipped".to_string(),
            });
        }
    };

    {
        let store = lock(store);
        write::owner_turn(&store, turn)?;
        match store.finish_onboarding(state) {
            Ok(true) => {}
            Ok(false) => {
                return Err(MemoryError {
                    code: E_ONBOARDING_STATE,
                    detail: "no onboarding is running".to_string(),
                });
            }
            Err(e) => {
                return Err(MemoryError {
                    code: E_MEMORY_HISTORY,
                    detail: e.code().to_string(),
                });
            }
        }
    }

    let refresh = state == OnboardingState::Done && request_refresh(home);
    Ok(serde_json::json!({"onboarding": state.as_str(), "refresh": refresh}).to_string())
}

/// Creates the memory refresh file (mode 0600); false, logged, when it cannot be written.
fn request_refresh(home: &MemoryHome) -> bool {
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(home.root().join(REFRESH_FILE));
    if written.is_err() {
        tracing::warn!(code = E_MEMORY_WRITE, "onboarding refresh not requested");
    }
    written.is_ok()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::{Value, json};

    use super::*;
    use crate::memory::E_MEMORY_ORIGIN;
    use crate::store::{Admitted, OnboardingRow};

    const KEY: &str = "p:th:111";

    struct Fx {
        _dir: tempfile::TempDir,
        store: Mutex<Store>,
        home: MemoryHome,
        turn: TurnContext,
    }

    impl Fx {
        fn start(&self) {
            lock(&self.store).start_onboarding().expect("start");
        }

        fn state(&self) -> Option<OnboardingState> {
            lock(&self.store)
                .onboarding()
                .expect("onboarding")
                .map(|row: OnboardingRow| row.state)
        }

        fn finish(
            &self,
            turn: Option<&TurnContext>,
            outcome: &str,
            reason: &str,
        ) -> Result<Value, MemoryError> {
            finish(&self.store, &self.home, turn, outcome, reason)
                .map(|out| serde_json::from_str(&out).expect("result json"))
        }

        fn refresh_file(&self) -> PathBuf {
            self.home.root().join(REFRESH_FILE)
        }
    }

    /// A store and a memory home in a temp dir, with owner row 1 (author 111) claimed.
    fn fx() -> Fx {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = Store::open(&dir.path().join("inbox.sqlite")).expect("store");
        let home = MemoryHome::new(dir.path());
        home.ensure_layout().expect("layout");
        let Admitted::New(row) = store
            .admit("p", "m1", KEY, "222", "111", "hello")
            .expect("admit")
        else {
            panic!("duplicate row");
        };
        assert_eq!(row, 1);
        assert!(store.claim(row).expect("claim"));
        Fx {
            _dir: dir,
            store: Mutex::new(store),
            home,
            turn: TurnContext {
                row_id: row,
                conversation_key: KEY.to_string(),
            },
        }
    }

    #[test]
    fn finish_needs_an_owner_turn() {
        let fx = fx();
        fx.start();
        let err = fx.finish(None, "done", "r").expect_err("no turn");
        assert_eq!(err.code, E_MEMORY_ORIGIN);
        assert_eq!(fx.state(), Some(OnboardingState::Active));
        assert!(!fx.refresh_file().exists());
    }

    #[test]
    fn finish_done_ends_and_requests_refresh() {
        let fx = fx();
        fx.start();
        let out = fx.finish(Some(&fx.turn), "done", "r").expect("done");
        assert_eq!(out, json!({"onboarding": "done", "refresh": true}));
        assert_eq!(fx.state(), Some(OnboardingState::Done));
        let meta = std::fs::metadata(fx.refresh_file()).expect("refresh file");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn finish_skipped_ends_without_refresh() {
        let fx = fx();
        fx.start();
        let out = fx.finish(Some(&fx.turn), "skipped", "r").expect("skipped");
        assert_eq!(out, json!({"onboarding": "skipped", "refresh": false}));
        assert_eq!(fx.state(), Some(OnboardingState::Skipped));
        assert!(!fx.refresh_file().exists());
    }

    #[test]
    fn finish_twice_is_refused() {
        let fx = fx();
        fx.start();
        fx.finish(Some(&fx.turn), "done", "r").expect("first");
        let err = fx.finish(Some(&fx.turn), "done", "r").expect_err("second");
        assert_eq!(err.code, E_ONBOARDING_STATE);
        assert_eq!(fx.state(), Some(OnboardingState::Done));
    }

    #[test]
    fn finish_without_onboarding_is_refused() {
        let fx = fx();
        let err = fx
            .finish(Some(&fx.turn), "done", "r")
            .expect_err("no onboarding");
        assert_eq!(err.code, E_ONBOARDING_STATE);
        assert_eq!(fx.state(), None);
        assert!(!fx.refresh_file().exists());
    }

    #[test]
    fn finish_args_are_checked() {
        let fx = fx();
        fx.start();
        let err = fx
            .finish(Some(&fx.turn), "later", "r")
            .expect_err("bad outcome");
        assert_eq!(err.code, E_MEMORY_ARGS);
        assert_eq!(fx.state(), Some(OnboardingState::Active));

        let expected = history::check_reason("").expect_err("empty reason").code;
        let err = fx
            .finish(Some(&fx.turn), "done", "")
            .expect_err("empty reason");
        assert_eq!(err.code, expected);
        assert_eq!(fx.state(), Some(OnboardingState::Active));
        assert!(!fx.refresh_file().exists());
    }

    #[test]
    fn request_file_is_under_the_home() {
        let home = Path::new("qa-mbhome");
        assert_eq!(request_file(home), home.join("onboarding.request"));
    }

    #[test]
    fn request_creates_a_private_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        request(dir.path()).expect("request");
        let meta = std::fs::metadata(request_file(dir.path())).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.len(), 0);
        request(dir.path()).expect("second request");
    }

    #[test]
    fn request_failure_is_coded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("plain-file");
        std::fs::write(&home, b"x").unwrap();
        let err = request(&home).expect_err("home is a file");
        assert_eq!(err.code, E_ONBOARDING_REQUEST);
    }
}
