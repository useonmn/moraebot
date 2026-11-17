//! Multi-file patches in the Codex patch format: parsing and applying hunks to one text.

use super::{MAX_PATCH_BYTES, MAX_PATCH_FILES};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File:";
const UPDATE: &str = "*** Update File:";
const DELETE: &str = "*** Delete File:";
const MOVE_TO: &str = "*** Move to:";
const END_OF_FILE: &str = "*** End of File";

/// One file operation of a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    Add {
        path: String,
        text: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
    Delete {
        path: String,
    },
}

/// One `@@` hunk: an optional anchor line to find first, its lines, and whether it must end at
/// the end of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub anchor: Option<String>,
    pub lines: Vec<HunkLine>,
    pub end_of_file: bool,
}

/// One line of a hunk: kept, removed or added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    Keep(String),
    Remove(String),
    Add(String),
}

/// What is wrong with a patch text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchProblem {
    TooLarge,
    NoBegin,
    NoEnd,
    Header,
    EmptyPath,
    AddLine,
    HunkLine,
    EmptyUpdate,
    NoFiles,
    TooManyFiles,
    CarriageReturn,
    AfterEnd,
}

impl PatchProblem {
    /// A stable lowercase name for refusal details ("too_large", "no_begin", ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::NoBegin => "no_begin",
            Self::NoEnd => "no_end",
            Self::Header => "header",
            Self::EmptyPath => "empty_path",
            Self::AddLine => "add_line",
            Self::HunkLine => "hunk_line",
            Self::EmptyUpdate => "empty_update",
            Self::NoFiles => "no_files",
            Self::TooManyFiles => "too_many_files",
            Self::CarriageReturn => "carriage_return",
            Self::AfterEnd => "after_end",
        }
    }
}

/// A parse failure at a 1-based patch line (0 for TooLarge and TooManyFiles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub problem: PatchProblem,
}

fn fail(line: usize, problem: PatchProblem) -> ParseError {
    ParseError { line, problem }
}

/// The operation being read, with the patch line of its header.
enum Open {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Update {
        header: usize,
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
        /// True while the last hunk still takes lines (not closed by `*** End of File`).
        in_hunk: bool,
    },
    Delete {
        path: String,
    },
}

impl Open {
    fn close(self) -> Result<FileOp, ParseError> {
        match self {
            Open::Add { path, lines } => {
                let text = lines.iter().map(|line| format!("{line}\n")).collect();
                Ok(FileOp::Add { path, text })
            }
            Open::Update {
                header,
                path,
                move_to,
                hunks,
                ..
            } => {
                if hunks.is_empty() && move_to.is_none() {
                    return Err(fail(header, PatchProblem::EmptyUpdate));
                }
                Ok(FileOp::Update {
                    path,
                    move_to,
                    hunks,
                })
            }
            Open::Delete { path } => Ok(FileOp::Delete { path }),
        }
    }
}

/// The path after `prefix`, trimmed; `None` when `line` is not that header.
fn header_path(line: &str, prefix: &str, at: usize) -> Option<Result<String, ParseError>> {
    let rest = line.strip_prefix(prefix)?;
    let path = rest.trim();
    Some(if path.is_empty() {
        Err(fail(at, PatchProblem::EmptyPath))
    } else {
        Ok(path.to_owned())
    })
}

/// The lines of `text`; a final `\n` does not make an extra empty line.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// The file operations of `text`, in patch order.
pub fn parse(text: &str) -> Result<Vec<FileOp>, ParseError> {
    if text.len() > MAX_PATCH_BYTES {
        return Err(fail(0, PatchProblem::TooLarge));
    }
    if let Some(at) = text.find('\r') {
        let line = text[..at].matches('\n').count() + 1;
        return Err(fail(line, PatchProblem::CarriageReturn));
    }
    let lines = split_lines(text);
    if lines.first() != Some(&BEGIN) {
        return Err(fail(1, PatchProblem::NoBegin));
    }
    let mut ops = Vec::new();
    let mut open: Option<Open> = None;
    for (index, &line) in lines.iter().enumerate().skip(1) {
        let at = index + 1;
        if line == END {
            if let Some(op) = open.take() {
                ops.push(op.close()?);
            }
            if ops.is_empty() {
                return Err(fail(at, PatchProblem::NoFiles));
            }
            if index + 1 < lines.len() {
                return Err(fail(at + 1, PatchProblem::AfterEnd));
            }
            return Ok(ops);
        }
        let started = [ADD, UPDATE, DELETE]
            .into_iter()
            .find_map(|prefix| header_path(line, prefix, at).map(|path| (prefix, path)));
        if let Some((prefix, path)) = started {
            if let Some(op) = open.take() {
                ops.push(op.close()?);
            }
            if ops.len() == MAX_PATCH_FILES {
                return Err(fail(0, PatchProblem::TooManyFiles));
            }
            let path = path?;
            open = Some(match prefix {
                ADD => Open::Add {
                    path,
                    lines: Vec::new(),
                },
                UPDATE => Open::Update {
                    header: at,
                    path,
                    move_to: None,
                    hunks: Vec::new(),
                    in_hunk: false,
                },
                _ => Open::Delete { path },
            });
            continue;
        }
        match open.as_mut() {
            Some(Open::Update {
                move_to,
                hunks,
                in_hunk,
                ..
            }) => {
                if let Some(target) = header_path(line, MOVE_TO, at) {
                    if move_to.is_some() || !hunks.is_empty() {
                        return Err(fail(at, PatchProblem::Header));
                    }
                    *move_to = Some(target?);
                } else if line == END_OF_FILE {
                    let Some(hunk) = hunks.last_mut().filter(|_| *in_hunk) else {
                        return Err(fail(at, PatchProblem::Header));
                    };
                    hunk.end_of_file = true;
                    *in_hunk = false;
                } else if line.starts_with("*** ") {
                    return Err(fail(at, PatchProblem::Header));
                } else if line == "@@" || line.starts_with("@@ ") {
                    let anchor = line.strip_prefix("@@ ").map(str::to_owned);
                    hunks.push(Hunk {
                        anchor,
                        lines: Vec::new(),
                        end_of_file: false,
                    });
                    *in_hunk = true;
                } else {
                    let rest = || line[1..].to_owned();
                    let hunk_line = match line.as_bytes().first() {
                        _ if !*in_hunk => None,
                        None => Some(HunkLine::Keep(String::new())),
                        Some(b' ') => Some(HunkLine::Keep(rest())),
                        Some(b'-') => Some(HunkLine::Remove(rest())),
                        Some(b'+') => Some(HunkLine::Add(rest())),
                        Some(_) => None,
                    };
                    let (Some(hunk_line), Some(hunk)) = (hunk_line, hunks.last_mut()) else {
                        return Err(fail(at, PatchProblem::HunkLine));
                    };
                    hunk.lines.push(hunk_line);
                }
            }
            _ if line.starts_with("*** ") => return Err(fail(at, PatchProblem::Header)),
            Some(Open::Add { lines, .. }) => match line.strip_prefix('+') {
                Some(rest) => lines.push(rest.to_owned()),
                None => return Err(fail(at, PatchProblem::AddLine)),
            },
            Some(Open::Delete { .. }) | None => return Err(fail(at, PatchProblem::HunkLine)),
        }
    }
    Err(fail(lines.len(), PatchProblem::NoEnd))
}

/// The first position at or after `from` where `old` matches `lines` under `same`; with
/// `end_of_file` the match must end at the last line.
fn find(
    lines: &[String],
    old: &[&str],
    from: usize,
    end_of_file: bool,
    same: fn(&str, &str) -> bool,
) -> Option<usize> {
    let last = lines.len().checked_sub(old.len())?;
    let matches = |start: usize| {
        lines[start..start + old.len()]
            .iter()
            .zip(old)
            .all(|(line, want)| same(line, want))
    };
    if end_of_file {
        return (last >= from && matches(last)).then_some(last);
    }
    (from..=last).find(|&start| matches(start))
}

fn exact(a: &str, b: &str) -> bool {
    a == b
}

fn trailing_space_insensitive(a: &str, b: &str) -> bool {
    a.trim_end() == b.trim_end()
}

/// `text` with `hunks` applied in order; `Err(k)` names the 1-based hunk that did not match.
pub fn apply_hunks(text: &str, hunks: &[Hunk]) -> Result<String, usize> {
    let mut lines: Vec<String> = split_lines(text).into_iter().map(str::to_owned).collect();
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let number = index + 1;
        if let Some(anchor) = &hunk.anchor {
            let anchor = [anchor.as_str()];
            let at = find(&lines, &anchor, cursor, false, exact)
                .or_else(|| find(&lines, &anchor, cursor, false, trailing_space_insensitive))
                .ok_or(number)?;
            cursor = at + 1;
        }
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Keep(text) | HunkLine::Remove(text) => Some(text.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        let new: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Keep(text) | HunkLine::Add(text) => Some(text.clone()),
                HunkLine::Remove(_) => None,
            })
            .collect();
        if old.is_empty() {
            lines.extend(new);
            cursor = lines.len();
            continue;
        }
        let at = find(&lines, &old, cursor, hunk.end_of_file, exact)
            .or_else(|| {
                find(
                    &lines,
                    &old,
                    cursor,
                    hunk.end_of_file,
                    trailing_space_insensitive,
                )
            })
            .ok_or(number)?;
        cursor = at + new.len();
        lines.splice(at..at + old.len(), new);
    }
    Ok(lines.iter().map(|line| format!("{line}\n")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep(text: &str) -> HunkLine {
        HunkLine::Keep(text.to_owned())
    }

    fn remove(text: &str) -> HunkLine {
        HunkLine::Remove(text.to_owned())
    }

    fn add(text: &str) -> HunkLine {
        HunkLine::Add(text.to_owned())
    }

    fn hunk(lines: Vec<HunkLine>) -> Hunk {
        Hunk {
            anchor: None,
            lines,
            end_of_file: false,
        }
    }

    fn err(line: usize, problem: PatchProblem) -> Result<Vec<FileOp>, ParseError> {
        Err(ParseError { line, problem })
    }

    #[test]
    fn parse_every_operation() {
        let text = [
            "*** Begin Patch",
            "*** Add File: notes/a.md",
            "+---",
            "+description: a",
            "+---",
            "+one",
            "*** Update File: notes/b.md",
            "*** Move to: notes/c.md",
            "@@ two",
            " keep",
            "-old",
            "+new",
            "@@",
            "+tail",
            "*** End of File",
            "*** Delete File: notes/d.md",
            "*** End Patch",
        ]
        .join("\n");
        let want = vec![
            FileOp::Add {
                path: "notes/a.md".into(),
                text: "---\ndescription: a\n---\none\n".into(),
            },
            FileOp::Update {
                path: "notes/b.md".into(),
                move_to: Some("notes/c.md".into()),
                hunks: vec![
                    Hunk {
                        anchor: Some("two".into()),
                        lines: vec![keep("keep"), remove("old"), add("new")],
                        end_of_file: false,
                    },
                    Hunk {
                        anchor: None,
                        lines: vec![add("tail")],
                        end_of_file: true,
                    },
                ],
            },
            FileOp::Delete {
                path: "notes/d.md".into(),
            },
        ];
        assert_eq!(parse(&text), Ok(want.clone()));
        assert_eq!(parse(&format!("{text}\n")), Ok(want));
    }

    #[test]
    fn parse_problems() {
        use PatchProblem::*;
        let cases: [(&str, usize, PatchProblem); 13] = [
            ("", 1, NoBegin),
            ("hello\n", 1, NoBegin),
            ("*** Begin Patch\n*** Add File: a.md\n+x\n", 3, NoEnd),
            (
                "*** Begin Patch\n*** Copy File: a.md\n*** End Patch",
                2,
                Header,
            ),
            (
                "*** Begin Patch\n*** Add File:  \n*** End Patch",
                2,
                EmptyPath,
            ),
            (
                "*** Begin Patch\n*** Add File: a.md\nx\n*** End Patch",
                3,
                AddLine,
            ),
            (
                "*** Begin Patch\n*** Update File: a.md\n@@\n?x\n*** End Patch",
                4,
                HunkLine,
            ),
            (
                "*** Begin Patch\n*** Update File: a.md\n x\n*** End Patch",
                3,
                HunkLine,
            ),
            (
                "*** Begin Patch\n*** Update File: a.md\n*** End Patch",
                2,
                EmptyUpdate,
            ),
            ("*** Begin Patch\n*** End Patch", 2, NoFiles),
            (
                "*** Begin Patch\r\n*** Delete File: a.md\r\n*** End Patch",
                1,
                CarriageReturn,
            ),
            (
                "*** Begin Patch\n*** Delete File: a.md\n*** End Patch\nmore",
                4,
                AfterEnd,
            ),
            (
                "*** Begin Patch\n*** Delete File: a.md\n*** End of File\n*** End Patch",
                3,
                Header,
            ),
        ];
        for (text, line, problem) in cases {
            assert_eq!(parse(text), err(line, problem), "case {text:?}");
        }

        let deletes = |count: usize| {
            let mut text = String::from("*** Begin Patch\n");
            for i in 0..count {
                text.push_str(&format!("*** Delete File: n{i}.md\n"));
            }
            text + "*** End Patch\n"
        };
        assert_eq!(parse(&deletes(MAX_PATCH_FILES + 1)), err(0, TooManyFiles));
        assert_eq!(
            parse(&deletes(MAX_PATCH_FILES)).map(|ops| ops.len()),
            Ok(MAX_PATCH_FILES)
        );

        let sized = |size: usize| {
            let head = "*** Begin Patch\n*** Add File: a.md\n";
            let tail = "*** End Patch\n";
            let fill = size - head.len() - tail.len() - "+\n".len();
            let text = format!("{head}+{}\n{tail}", "y".repeat(fill));
            assert_eq!(text.len(), size);
            text
        };
        assert_eq!(parse(&sized(MAX_PATCH_BYTES + 1)), err(0, TooLarge));
        let ops = parse(&sized(MAX_PATCH_BYTES)).expect("a patch of exactly the limit parses");
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn problem_names_are_stable() {
        use PatchProblem::*;
        let names = [
            (TooLarge, "too_large"),
            (NoBegin, "no_begin"),
            (NoEnd, "no_end"),
            (Header, "header"),
            (EmptyPath, "empty_path"),
            (AddLine, "add_line"),
            (HunkLine, "hunk_line"),
            (EmptyUpdate, "empty_update"),
            (NoFiles, "no_files"),
            (TooManyFiles, "too_many_files"),
            (CarriageReturn, "carriage_return"),
            (AfterEnd, "after_end"),
        ];
        for (problem, name) in names {
            assert_eq!(problem.name(), name);
        }
        let distinct: std::collections::HashSet<_> = names.iter().map(|(p, _)| p.name()).collect();
        assert_eq!(distinct.len(), names.len());
    }

    #[test]
    fn apply_exact_and_in_order() {
        let hunks = [
            hunk(vec![keep("b"), remove("c"), add("x")]),
            hunk(vec![remove("b"), add("y")]),
        ];
        assert_eq!(
            apply_hunks("a\nb\nc\nb\nd\n", &hunks),
            Ok("a\nb\nx\ny\nd\n".to_owned())
        );
    }

    #[test]
    fn apply_trailing_space_fallback() {
        assert_eq!(
            apply_hunks("a  \nb\n", &[hunk(vec![remove("a"), add("z")])]),
            Ok("z\nb\n".to_owned())
        );
        assert_eq!(apply_hunks("a  \nb\n", &[hunk(vec![remove(" a")])]), Err(1));
    }

    #[test]
    fn apply_anchor() {
        let anchored = |anchor: &str| Hunk {
            anchor: Some(anchor.to_owned()),
            lines: vec![remove("k"), add("m")],
            end_of_file: false,
        };
        let text = "[x]\nk\n[y]\nk\n";
        assert_eq!(
            apply_hunks(text, &[anchored("[y]")]),
            Ok("[x]\nk\n[y]\nm\n".to_owned())
        );
        assert_eq!(apply_hunks(text, &[anchored("[z]")]), Err(1));
    }

    #[test]
    fn apply_end_of_file() {
        let last = Hunk {
            anchor: None,
            lines: vec![remove("k"), add("m")],
            end_of_file: true,
        };
        assert_eq!(apply_hunks("k\nk\n", &[last]), Ok("k\nm\n".to_owned()));
    }

    #[test]
    fn apply_append_hunk() {
        for text in ["a\n", "a"] {
            assert_eq!(
                apply_hunks(text, &[hunk(vec![add("b")])]),
                Ok("a\nb\n".to_owned()),
                "case {text:?}"
            );
        }
    }

    #[test]
    fn apply_reports_the_failing_hunk() {
        let hunks = [
            hunk(vec![remove("a"), add("x")]),
            hunk(vec![remove("missing")]),
            hunk(vec![remove("c"), add("z")]),
        ];
        assert_eq!(apply_hunks("a\nb\nc\n", &hunks), Err(2));
    }
}
