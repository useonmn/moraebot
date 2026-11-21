//! Owner status questions answered by moraebot itself (#68), so the owner gets an answer
//! while the lead is busy. The matcher is rule-based and conservative: only the whole message,
//! after trimming whitespace and end punctuation and ignoring case, compared with a fixed list.
//! "status of PR 12?" or "뭐 하고 있어? 그거 말고 X 해줘" still go to the lead.
//!
//! The answer is built from the lead's turn state ([`LeadStatus`]) and the running lanes. The
//! lead's current step is shown only when its turn belongs to the asking conversation; a turn
//! of another conversation is named without details, and lanes of other conversations are only
//! counted, so a shared channel never sees another conversation's work.

use std::time::Duration;

use crate::lane::news_text;
use crate::omp::LeadStatus;

/// The language a status question was asked in; the answer uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Korean,
    English,
}

const KOREAN: &[&str] = &[
    "뭐 하고 있어",
    "뭐하고 있어",
    "뭐 하고있어",
    "뭐하고있어",
    "지금 뭐 하고 있어",
    "지금 뭐하고 있어",
    "뭐해",
    "뭐 해",
    "지금 뭐해",
    "지금 뭐 해",
    "상태",
    "지금 상태",
];

const ENGLISH: &[&str] = &[
    "status",
    "what are you doing",
    "what're you doing",
    "what are you doing now",
];

/// A task's first line is cut to this many characters in a status answer.
const TASK_CHARS: usize = 80;

/// The language of `text` when the whole message is a status question; `None` otherwise.
pub fn request(text: &str) -> Option<Lang> {
    let trimmed = text.trim().trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '?' | '!' | '.' | '~' | '？' | '！' | '。')
    });
    let words = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let words = words.to_lowercase();
    if KOREAN.contains(&words.as_str()) {
        Some(Lang::Korean)
    } else if ENGLISH.contains(&words.as_str()) {
        Some(Lang::English)
    } else {
        None
    }
}

/// One running lane of the asking conversation.
pub struct LaneLine<'a> {
    pub id: i64,
    /// How long its current job has run; `None` when moraebot does not know.
    pub running: Option<Duration>,
    /// The task the lead gave it.
    pub task: &'a str,
}

/// The status answer for conversation `key`: the lead's turn (`lead`), the running lanes of
/// this conversation (`lanes`) and the number of running lanes of other conversations.
pub fn reply(
    lang: Lang,
    key: &str,
    lead: Option<&LeadStatus>,
    lanes: &[LaneLine],
    others: usize,
) -> String {
    let mut lines = Vec::new();
    match lead {
        Some(turn) if turn.conversation == key => {
            let minutes = minutes(turn.running);
            lines.push(match lang {
                Lang::Korean => format!("이 대화 일을 {minutes}분째 하고 있어요"),
                Lang::English => format!("Working on this conversation for {minutes} min."),
            });
            if let Some((name, intent)) = &turn.tool {
                let step = intent.as_deref().unwrap_or(name);
                lines.push(match lang {
                    Lang::Korean => format!("지금 단계: {step}"),
                    Lang::English => format!("Current step: {step}"),
                });
            }
        }
        Some(turn) => {
            let minutes = minutes(turn.running);
            lines.push(match lang {
                Lang::Korean => format!("다른 대화 일을 {minutes}분째 하고 있어요"),
                Lang::English => {
                    format!("Working on another conversation for {minutes} min.")
                }
            });
        }
        None => lines.push(
            match lang {
                Lang::Korean => "지금 하고 있는 일은 없어요",
                Lang::English => "Not working on anything right now.",
            }
            .to_owned(),
        ),
    }
    if !lanes.is_empty() {
        lines.push(match lang {
            Lang::Korean => format!("이 대화의 백그라운드 작업 {}개:", lanes.len()),
            Lang::English => format!("Background lanes of this conversation: {}", lanes.len()),
        });
        for lane in lanes {
            let task = first_line(lane.task);
            lines.push(match (lang, lane.running) {
                (Lang::Korean, Some(running)) => {
                    format!("- lane {} ({}분째): {task}", lane.id, minutes(running))
                }
                (Lang::English, Some(running)) => {
                    format!("- lane {} ({} min): {task}", lane.id, minutes(running))
                }
                (_, None) => format!("- lane {}: {task}", lane.id),
            });
        }
    }
    if others > 0 {
        lines.push(match lang {
            Lang::Korean => format!("다른 대화의 백그라운드 작업도 {others}개 돌고 있어요"),
            Lang::English => format!("{others} more lanes run for other conversations."),
        });
    }
    if lanes.is_empty() && others == 0 {
        lines.push(
            match lang {
                Lang::Korean => "돌고 있는 백그라운드 작업은 없어요",
                Lang::English => "No background lanes are running.",
            }
            .to_owned(),
        );
    }
    lines.join("\n")
}

/// Whole minutes, at least one.
fn minutes(running: Duration) -> u64 {
    (running.as_secs() / 60).max(1)
}

/// The task's first non-empty line, cleaned like a lane note and cut to [`TASK_CHARS`].
fn first_line(task: &str) -> String {
    let line = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = news_text(line, &[]);
    if line.chars().count() <= TASK_CHARS {
        return line;
    }
    let cut: String = line.chars().take(TASK_CHARS).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_whole_status_questions_match() {
        for (text, lang) in [
            ("뭐 하고 있어?", Lang::Korean),
            ("  뭐해  ", Lang::Korean),
            ("뭐하고있어??", Lang::Korean),
            ("상태", Lang::Korean),
            ("Status", Lang::English),
            ("what are you doing?", Lang::English),
            ("What  are you   doing!", Lang::English),
        ] {
            assert_eq!(request(text), Some(lang), "{text}");
        }
        for text in [
            "status of PR 12?",
            "뭐 하고 있어? 그거 말고 X 해줘",
            "뭐해 이거",
            "what are you doing with the build",
            "",
            "?",
        ] {
            assert_eq!(request(text), None, "{text}");
        }
    }

    fn turn(key: &str, tool: Option<(&str, Option<&str>)>) -> LeadStatus {
        LeadStatus {
            conversation: key.to_owned(),
            running: Duration::from_secs(7 * 60 + 5),
            tool: tool.map(|(name, intent)| (name.to_owned(), intent.map(str::to_owned))),
        }
    }

    #[test]
    fn the_answer_shows_this_conversations_turn_and_lanes() {
        let lead = turn("p:th:1", Some(("bash", Some("running the tests"))));
        let lanes = [LaneLine {
            id: 4,
            running: Some(Duration::from_secs(30)),
            task: "\nfix the build\nmore detail",
        }];
        assert_eq!(
            reply(Lang::English, "p:th:1", Some(&lead), &lanes, 2),
            "Working on this conversation for 7 min.\nCurrent step: running the tests\n\
             Background lanes of this conversation: 1\n- lane 4 (1 min): fix the build\n\
             2 more lanes run for other conversations."
        );
        let no_intent = turn("p:th:1", Some(("read", None)));
        assert_eq!(
            reply(Lang::Korean, "p:th:1", Some(&no_intent), &[], 0),
            "이 대화 일을 7분째 하고 있어요\n지금 단계: read\n돌고 있는 백그라운드 작업은 없어요"
        );
    }

    #[test]
    fn another_conversations_turn_is_named_without_details() {
        let lead = turn("p:th:2", Some(("bash", Some("secret plan"))));
        let text = reply(Lang::English, "p:th:1", Some(&lead), &[], 0);
        assert_eq!(
            text,
            "Working on another conversation for 7 min.\nNo background lanes are running."
        );
        assert_eq!(
            reply(Lang::Korean, "p:th:1", None, &[], 1),
            "지금 하고 있는 일은 없어요\n다른 대화의 백그라운드 작업도 1개 돌고 있어요"
        );
    }

    #[test]
    fn a_long_task_line_is_cut() {
        let task = "x".repeat(200);
        let line = first_line(&task);
        assert_eq!(line.chars().count(), TASK_CHARS + 1);
        assert!(line.ends_with('…'));
    }
}
