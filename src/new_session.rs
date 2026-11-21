//! The owner's "new session" command (#74): the lead starts over with an empty omp session while
//! the gateway, the lanes and the memory stay. The matcher is rule-based and conservative: only
//! the whole message, after trimming whitespace and a closing `.` or `!` and ignoring case,
//! compared with a fixed list. "새 세션 열어줄래?" still goes to the lead.
//!
//! The switch itself is [`crate::omp::OmpManager::new_session`]; the dispatcher answers with the
//! texts below in the language the command was given in.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::memory::Day;
use crate::status::Lang;

/// How long a confirmation question stays open.
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(10 * 60);

const KOREAN: &[&str] = &["새 세션", "새 세션 시작", "새 세션으로 시작"];
const ENGLISH: &[&str] = &["new session", "/new"];
const KOREAN_CONFIRM: &str = "새 세션 확인";
const ENGLISH_CONFIRM: &str = "new session confirm";

/// A matched command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub lang: Lang,
    /// The owner confirms an earlier question (`새 세션 확인`, `new session confirm`).
    pub confirm: bool,
}

/// The command in `text` when the whole message is one; `None` otherwise.
pub fn request(text: &str) -> Option<Request> {
    let trimmed = text
        .trim()
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '!' | '。' | '！'));
    let words = trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let (lang, confirm) = match words.as_str() {
        KOREAN_CONFIRM => (Lang::Korean, true),
        ENGLISH_CONFIRM => (Lang::English, true),
        w if KOREAN.contains(&w) => (Lang::Korean, false),
        w if ENGLISH.contains(&w) => (Lang::English, false),
        _ => return None,
    };
    Some(Request { lang, confirm })
}

/// The question asked while `lanes` lanes run; nothing was changed.
pub fn confirm_text(lang: Lang, lanes: usize) -> String {
    let minutes = CONFIRM_WINDOW.as_secs() / 60;
    match lang {
        Lang::Korean => format!(
            "백그라운드 작업 {lanes}개가 돌고 있어요. 새 세션을 열어도 작업은 계속되고 결과는 새 lead가 받지만, 새 lead는 지금까지의 대화를 기억하지 못해요. 그래도 열려면 {minutes}분 안에 `{KOREAN_CONFIRM}`이라고 보내 주세요."
        ),
        Lang::English => format!(
            "{lanes} background lane(s) are running. They keep running and the new lead gets their results, but it won't remember this conversation. To open a new session anyway, send `{ENGLISH_CONFIRM}` within {minutes} minutes."
        ),
    }
}

/// The answer after the switch; `backup` is the old session's folder as shown to the owner, `None`
/// when there was no session folder to keep.
pub fn opened_text(lang: Lang, backup: Option<&str>) -> String {
    match (lang, backup) {
        (Lang::Korean, Some(path)) => {
            format!("새 세션을 열었어요. 기억은 그대로이고, 이전 세션은 {path}에 백업했어요.")
        }
        (Lang::Korean, None) => "새 세션을 열었어요. 기억은 그대로예요.".to_owned(),
        (Lang::English, Some(path)) => format!(
            "Opened a new session. Memory is kept; the old session was backed up to {path}."
        ),
        (Lang::English, None) => "Opened a new session. Memory is kept.".to_owned(),
    }
}

/// The answer when the switch failed; `code` is the stable error code.
pub fn failed_text(lang: Lang, code: &str) -> String {
    match lang {
        Lang::Korean => {
            format!("새 세션을 열지 못했어요 ({code}). lead는 이전 세션으로 계속해요.")
        }
        Lang::English => {
            format!("Could not open a new session ({code}). The lead keeps its old session.")
        }
    }
}

/// A free backup folder next to `session_dir`: `session-backup-<UTC YYYYMMDDTHHMMSSZ>`, with
/// `-2`, `-3`, ... added when that name is taken.
pub fn backup_dir(session_dir: &Path, now: SystemTime) -> PathBuf {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let (y, mo, d) = Day::from_ymd(1970, 1, 1)
        .expect("the epoch is a date")
        .plus_days(secs.div_euclid(86_400))
        .ymd();
    let day_secs = secs.rem_euclid(86_400);
    let (h, mi, s) = (day_secs / 3600, day_secs % 3600 / 60, day_secs % 60);
    let name = format!("session-backup-{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z");
    let parent = session_dir.parent().unwrap_or(session_dir);
    let mut candidate = parent.join(&name);
    let mut n = 2;
    while std::fs::symlink_metadata(&candidate).is_ok() {
        candidate = parent.join(format!("{name}-{n}"));
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_whole_commands_match() {
        let korean = Some(Request {
            lang: Lang::Korean,
            confirm: false,
        });
        let english = Some(Request {
            lang: Lang::English,
            confirm: false,
        });
        for text in [
            "새 세션",
            " 새  세션 ",
            "새 세션으로 시작",
            "새 세션 시작!",
            "새 세션.",
        ] {
            assert_eq!(request(text), korean, "{text}");
        }
        for text in ["new session", "New Session", "/new", "/NEW"] {
            assert_eq!(request(text), english, "{text}");
        }
        assert_eq!(
            request("새 세션 확인"),
            Some(Request {
                lang: Lang::Korean,
                confirm: true
            })
        );
        assert_eq!(
            request("new session confirm"),
            Some(Request {
                lang: Lang::English,
                confirm: true
            })
        );
        for text in [
            "새 세션 열어줄래?",
            "새 세션?",
            "새세션",
            "new session please",
            "/new session",
            "start a new session",
            "",
        ] {
            assert_eq!(request(text), None, "{text}");
        }
    }

    #[test]
    fn backup_dir_is_named_by_utc_time_and_never_reused() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session");
        // 2026-11-21 10:53:07 UTC.
        let at = UNIX_EPOCH + Duration::from_secs(1_795_258_387);
        let first = backup_dir(&session, at);
        assert_eq!(first, root.path().join("session-backup-20261121T105307Z"));
        std::fs::create_dir(&first).unwrap();
        assert_eq!(
            backup_dir(&session, at),
            root.path().join("session-backup-20261121T105307Z-2")
        );
    }
}
