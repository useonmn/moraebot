//! The first meeting with the owner: the request file, the mark on the lead's input and the lead's
//! finish tool.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::lead;

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
