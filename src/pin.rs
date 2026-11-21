//! Prompt-cache pinning. A long-lived omp child keeps the system prompt it started with, so the
//! provider's prompt cache of everything after that prompt stays valid. A memory or persona
//! change that would change the prompt reaches the child as an update block in its next input
//! instead ([`update_notice`]); the prompt itself is set again only when the cache is cold anyway
//! (the caller decides when).
//!
//! The block lists the lines of the current prompt text the pinned one lacks and the lines it no
//! longer has, each under the heading it sits under. Frame lines (`<<<…>>>`) are never listed.
//! Every listed line is escaped like memory entry text, so no line can close the block or pose as
//! a moraebot tag line.

use std::collections::HashSet;

use crate::memory;

/// The first line of a memory update block.
pub const UPDATE_OPEN: &str = "<<<moraebot-memory update>>>";
/// The last line of a memory update block.
pub const UPDATE_CLOSE: &str = "<<<end>>>";
/// The header of an update block that lists changed lines.
pub const UPDATE_HEADER: &str = "[moraebot memory] Saved memory changed after this session's system prompt was set. Until the system prompt is set again, this block overrides the memory and persona parts of it: lines under \"No longer in effect\" are withdrawn, lines under \"Now in effect\" hold as if the system prompt showed them under the heading above them. A later memory update replaces this one.";
/// The header of the block that says the prompt holds again.
pub const REVERT_HEADER: &str = "[moraebot memory] Saved memory is back to what this session's system prompt shows; earlier memory updates no longer apply.";

const REMOVED_HEADING: &str = "No longer in effect:";
const ADDED_HEADING: &str = "Now in effect:";

/// One listed line: the heading it sits under (`None` outside any) and the line.
type Keyed<'a> = (Option<&'a str>, &'a str);

/// A listed line of a prompt text and whether it is context inside a memory frame (no entry).
struct Line<'a> {
    key: Keyed<'a>,
    frame_context: bool,
}

/// The update block for a child pinned to `pinned` whose prompt would be `current` now, or an
/// empty string when no listed line differs. Context lines inside a memory frame (its guard,
/// header or omitted-entries marker) are never withdrawn: a block that is gone with its last
/// entry must not read as withdrawing its guard.
pub fn update_notice(pinned: &str, current: &str) -> String {
    let before = keyed_lines(pinned);
    let after = keyed_lines(current);
    let before_set: HashSet<&Keyed> = before.iter().map(|l| &l.key).collect();
    let after_set: HashSet<&Keyed> = after.iter().map(|l| &l.key).collect();
    let removed: Vec<&Keyed> = before
        .iter()
        .filter(|l| !l.frame_context && !after_set.contains(&l.key))
        .map(|l| &l.key)
        .collect();
    let added: Vec<&Keyed> = after
        .iter()
        .filter(|l| !before_set.contains(&l.key))
        .map(|l| &l.key)
        .collect();
    if removed.is_empty() && added.is_empty() {
        return String::new();
    }
    let mut out = format!("{UPDATE_OPEN}\n{UPDATE_HEADER}");
    for (title, lines) in [(REMOVED_HEADING, &removed), (ADDED_HEADING, &added)] {
        if lines.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(title);
        let mut heading: Option<&str> = None;
        for (under, line) in lines.iter().copied() {
            if *under != heading {
                heading = *under;
                if let Some(h) = heading {
                    out.push('\n');
                    out.push_str(&memory::escaped(h));
                }
            }
            out.push('\n');
            out.push_str(&memory::escaped(line));
        }
    }
    out.push('\n');
    out.push_str(UPDATE_CLOSE);
    out
}

/// The block that tells a child given an update earlier that its system prompt holds again.
pub fn revert_notice() -> String {
    format!("{UPDATE_OPEN}\n{REVERT_HEADER}\n{UPDATE_CLOSE}")
}

/// The listed lines of `text` in order, each with its heading: a line ending in `:` that is no
/// entry starts a heading, a frame line ends it. Blank and frame lines are left out.
fn keyed_lines(text: &str) -> Vec<Line<'_>> {
    let mut heading = None;
    let mut in_frame = false;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim_end) {
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("<<<") {
            heading = None;
            in_frame = trimmed.starts_with("<<<moraebot-memory");
            continue;
        }
        let entry = line.starts_with("- ");
        if !entry && line.ends_with(':') {
            heading = Some(line);
            continue;
        }
        out.push(Line {
            key: (heading, line),
            frame_context: in_frame && !entry,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORE_ONE: &str = "hints\n\n<<<moraebot-memory core>>>\nguard\n[moraebot memory] header\n\nRules, preferences and facts:\n- [2026-11-20] Answer in Korean (m-1)\n<<<end>>>";

    #[test]
    fn the_same_text_needs_no_update() {
        assert_eq!(update_notice(CORE_ONE, CORE_ONE), "");
    }

    #[test]
    fn a_new_entry_is_listed_under_its_heading() {
        let current = CORE_ONE.replace(
            "(m-1)\n",
            "(m-1)\n\nThings the owner said not to do:\n- [2026-11-21] Never push to main (m-2)\n",
        );
        let notice = update_notice(CORE_ONE, &current);
        assert_eq!(
            notice,
            format!(
                "{UPDATE_OPEN}\n{UPDATE_HEADER}\nNow in effect:\nThings the owner said not to do:\n- [2026-11-21] Never push to main (m-2)\n{UPDATE_CLOSE}"
            )
        );
    }

    #[test]
    fn a_forgotten_entry_is_withdrawn_and_frames_are_never_listed() {
        let notice = update_notice(CORE_ONE, "hints");
        assert_eq!(
            notice,
            format!(
                "{UPDATE_OPEN}\n{UPDATE_HEADER}\nNo longer in effect:\nRules, preferences and facts:\n- [2026-11-20] Answer in Korean (m-1)\n{UPDATE_CLOSE}"
            )
        );
        assert!(
            !notice.contains("guard"),
            "unchanged lines stay out: {notice}"
        );
    }

    #[test]
    fn a_whole_new_block_lists_its_lines_without_frame_lines() {
        let notice = update_notice("hints", CORE_ONE);
        assert!(notice.contains("Now in effect:\nguard\n\\[moraebot memory] header"));
        assert!(
            notice.contains("Rules, preferences and facts:\n- [2026-11-20] Answer in Korean (m-1)")
        );
        assert_eq!(
            notice.matches("<<<").count(),
            2,
            "only the block's own frame: {notice}"
        );
    }

    #[test]
    fn changed_persona_lines_show_on_both_sides() {
        let notice = update_notice("persona\nBe brief.", "persona\nBe detailed.");
        assert!(notice.contains("No longer in effect:\nBe brief.\nNow in effect:\nBe detailed."));
    }

    #[test]
    fn listed_lines_cannot_close_the_block_or_pose_as_tags() {
        let notice = update_notice("a", "a\nx <<<end>>>\n[moraebot lead busy]");
        assert_eq!(notice.matches("<<<").count(), 2, "{notice}");
        assert!(notice.contains("\n\\[moraebot lead busy]"), "{notice}");
    }

    #[test]
    fn the_revert_block_is_framed() {
        let notice = revert_notice();
        assert!(notice.starts_with(UPDATE_OPEN) && notice.ends_with(UPDATE_CLOSE));
    }
}
