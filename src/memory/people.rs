//! Person records: card and observation file formats, slugs, and dated observations with a repeat count.

use super::agreements::normalize;
use super::frontmatter::{self, Frontmatter, render_frontmatter};
use super::{
    CARD_FILE, CARD_PREFIXES, EXPLICIT_SECTION, HUMAN_SLUG, MAX_PERSON_ENTRIES,
    MAX_PERSON_LINE_CHARS, MAX_SLUG_CHARS, OBSERVATIONS_FILE, PEOPLE_DIR,
};

/// Which person file a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonFile {
    Card,
    Observations,
}

/// What is wrong with a person file or slug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    Slug,
    Reserved,
    Kind,
    Prefix,
    Relationship,
    Section,
    Entry,
    TooLong,
    TooMany,
}

impl ProblemKind {
    /// A stable lowercase name for refusal details: "slug", "reserved", "kind", "prefix",
    /// "relationship", "section", "entry", "too_long", "too_many".
    pub fn name(self) -> &'static str {
        match self {
            Self::Slug => "slug",
            Self::Reserved => "reserved",
            Self::Kind => "kind",
            Self::Prefix => "prefix",
            Self::Relationship => "relationship",
            Self::Section => "section",
            Self::Entry => "entry",
            Self::TooLong => "too_long",
            Self::TooMany => "too_many",
        }
    }
}

/// One problem: the 1-based line of the whole file (frontmatter included; 0 for a slug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Problem {
    pub line: usize,
    pub kind: ProblemKind,
}

fn problem(line: usize, kind: ProblemKind) -> Problem {
    Problem { line, kind }
}

/// `[a-z0-9][a-z0-9-]*`, at most MAX_SLUG_CHARS.
pub fn check_slug(slug: &str) -> Result<(), Problem> {
    let mut chars = slug.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if first_ok && rest_ok && slug.chars().count() <= MAX_SLUG_CHARS {
        Ok(())
    } else {
        Err(problem(0, ProblemKind::Slug))
    }
}

/// `people/<slug>/card.md` or `people/<slug>/observations.md` (exactly three components): the
/// slug and the file; any other path gives None.
pub fn classify(path: &str) -> Option<(&str, PersonFile)> {
    let mut parts = path.split('/');
    let (dir, slug, file) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || dir != PEOPLE_DIR {
        return None;
    }
    match file {
        CARD_FILE => Some((slug, PersonFile::Card)),
        OBSERVATIONS_FILE => Some((slug, PersonFile::Observations)),
        _ => None,
    }
}

/// True for any path under `people/human/`.
pub fn is_reserved(path: &str) -> bool {
    path.strip_prefix(PEOPLE_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.strip_prefix(HUMAN_SLUG))
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The frontmatter of `text`, the number of lines it spans, and the body lines.
fn split(text: &str) -> Option<(Frontmatter, usize, Vec<&str>)> {
    let parsed = frontmatter::parse(text).ok()?;
    let head = &text[..text.len() - parsed.body.len()];
    let lines = parsed.body.split_inclusive('\n').collect();
    Some((parsed.frontmatter, head.matches('\n').count(), lines))
}

fn bare(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

fn too_long(line: &str) -> bool {
    line.chars().count() > MAX_PERSON_LINE_CHARS
}

/// Every problem of a person file text, in line order (empty when the text is valid).
pub fn check(file: PersonFile, text: &str) -> Vec<Problem> {
    let Some((fm, offset, lines)) = split(text) else {
        return vec![problem(1, ProblemKind::Kind)];
    };
    let mut problems = Vec::new();
    if file == PersonFile::Card && fm.get("kind") != Some("person") {
        problems.push(problem(1, ProblemKind::Kind));
    }
    let mut entries = 0;
    let mut in_section = false;
    for (index, raw) in lines.iter().enumerate() {
        let line = bare(raw);
        let no = offset + index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let counted = match file {
            PersonFile::Card => card_line(line, no, &mut problems),
            PersonFile::Observations => observation_line(line, no, &mut in_section, &mut problems),
        };
        if too_long(line) {
            problems.push(problem(no, ProblemKind::TooLong));
        }
        if counted {
            entries += 1;
            if entries > MAX_PERSON_ENTRIES {
                problems.push(problem(no, ProblemKind::TooMany));
            }
        }
    }
    problems
}

/// Check one non-blank card line; true when it starts with a card prefix (it counts).
fn card_line(line: &str, no: usize, problems: &mut Vec<Problem>) -> bool {
    let Some((prefix, rest)) = CARD_PREFIXES
        .iter()
        .find_map(|p| line.strip_prefix(p).map(|rest| (*p, rest)))
    else {
        problems.push(problem(no, ProblemKind::Prefix));
        return false;
    };
    if rest.trim().is_empty() {
        problems.push(problem(no, ProblemKind::Prefix));
    } else if prefix == CARD_PREFIXES[2] && !relationship_ok(rest) {
        problems.push(problem(no, ProblemKind::Relationship));
    }
    true
}

/// `<predicate>: <slug>` with a non-blank predicate without `:` and a valid slug.
fn relationship_ok(rest: &str) -> bool {
    rest.split_once(": ").is_some_and(|(predicate, slug)| {
        !predicate.trim().is_empty() && !predicate.contains(':') && check_slug(slug).is_ok()
    })
}

/// Check one non-blank observations line; true when it is a valid entry (it counts).
fn observation_line(
    line: &str,
    no: usize,
    in_section: &mut bool,
    problems: &mut Vec<Problem>,
) -> bool {
    if line == EXPLICIT_SECTION {
        *in_section = true;
        return false;
    }
    if line.starts_with("## ") {
        problems.push(problem(no, ProblemKind::Section));
        return false;
    }
    if parse_entry(line).is_none() {
        problems.push(problem(no, ProblemKind::Entry));
        return false;
    }
    if !*in_section {
        problems.push(problem(no, ProblemKind::Section));
        return false;
    }
    true
}

/// One parsed `- [YYYY-MM-DD] <text> <!-- n=<N> -->` entry.
struct Entry<'a> {
    text: &'a str,
    count: i64,
}

fn parse_entry(line: &str) -> Option<Entry<'_>> {
    let rest = line.strip_prefix("- [")?;
    let date = rest.get(..10)?;
    let rest = rest.get(10..)?.strip_prefix("] ")?;
    let (text, tail) = rest.rsplit_once(" <!-- n=")?;
    let digits = tail.strip_suffix(" -->")?;
    let count_ok = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0');
    if !real_date(date) || !fact_ok(text) || !count_ok {
        return None;
    }
    Some(Entry {
        text,
        count: digits.parse().ok()?,
    })
}

fn fact_ok(text: &str) -> bool {
    !text.trim().is_empty() && !text.contains("<!--") && !text.contains("-->")
}

/// A real calendar date written `YYYY-MM-DD`.
fn real_date(date: &str) -> bool {
    let b = date.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = &date[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return false;
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn entry_line(date: &str, text: &str, count: i64) -> Result<String, Problem> {
    let line = format!("- [{date}] {text} <!-- n={count} -->");
    if too_long(&line) {
        return Err(problem(0, ProblemKind::TooLong));
    }
    Ok(line)
}

/// A new card: frontmatter `description: About <name>`, `kind: person`, `aliases: <aliases>`
/// when given, then `IDENTITY: <name>` when a name is given. `<name>` falls back to the slug.
pub fn new_card(slug: &str, name: Option<&str>, aliases: Option<&str>) -> String {
    let mut fm = Frontmatter::with_description(&format!("About {}", name.unwrap_or(slug)));
    fm.set("kind", "person");
    if let Some(aliases) = aliases {
        fm.set("aliases", aliases);
    }
    let mut text = render_frontmatter(&fm);
    if let Some(name) = name {
        text.push_str(&format!("{}{name}\n", CARD_PREFIXES[0]));
    }
    text
}

/// A new observations file: frontmatter `description: Facts about <name>`, then the heading.
pub fn new_observations(slug: &str, name: Option<&str>) -> String {
    let fm = Frontmatter::with_description(&format!("Facts about {}", name.unwrap_or(slug)));
    format!("{}{EXPLICIT_SECTION}\n", render_frontmatter(&fm))
}

/// `text` with the fact recorded under EXPLICIT_SECTION: an entry with the same
/// `agreements::normalize` text gets `date` and `n+1`; otherwise a new entry with n=1 goes after
/// the section's last non-blank line (the section is appended at the end of the body when
/// missing). The section runs from the first body line exactly equal to EXPLICIT_SECTION up to
/// (not including) the next line that starts with `## ` or `# `, or the end of the body; a
/// second `## Explicit` heading ends the first section and is ignored (only the first counts).
/// Repeat matching looks only inside that section. Returns the new text and the entry's count.
pub fn observe(text: &str, fact: &str, date: &str) -> Result<(String, i64), Problem> {
    if fact.contains('\n') || !fact_ok(fact) {
        return Err(problem(0, ProblemKind::Entry));
    }
    let fact = fact.trim();
    let parsed = frontmatter::parse(text).map_err(|_| problem(1, ProblemKind::Kind))?;
    let head = &text[..text.len() - parsed.body.len()];
    let mut lines: Vec<String> = parsed
        .body
        .split_inclusive('\n')
        .map(str::to_string)
        .collect();
    let Some(start) = lines.iter().position(|l| bare(l) == EXPLICIT_SECTION) else {
        let entry = entry_line(date, fact, 1)?;
        let mut body = parsed.body.to_string();
        if !body.is_empty() && !body.ends_with("\n\n") {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push('\n');
        }
        return Ok((format!("{head}{body}{EXPLICIT_SECTION}\n{entry}\n"), 1));
    };
    let end = (start + 1..lines.len())
        .find(|&i| lines[i].starts_with("## ") || lines[i].starts_with("# "))
        .unwrap_or(lines.len());
    let wanted = normalize(fact);
    let mut entries = 0;
    for i in start + 1..end {
        let Some(entry) = parse_entry(bare(&lines[i])) else {
            continue;
        };
        entries += 1;
        if normalize(entry.text) == wanted {
            let count = entry
                .count
                .checked_add(1)
                .ok_or(problem(0, ProblemKind::Entry))?;
            let newline = if lines[i].ends_with('\n') { "\n" } else { "" };
            lines[i] = format!("{}{newline}", entry_line(date, entry.text, count)?);
            return Ok((format!("{head}{}", lines.concat()), count));
        }
    }
    if entries >= MAX_PERSON_ENTRIES {
        return Err(problem(0, ProblemKind::TooMany));
    }
    let entry = entry_line(date, fact, 1)?;
    let last = (start..end)
        .rev()
        .find(|&i| !lines[i].trim().is_empty())
        .unwrap_or(start);
    if !lines[last].ends_with('\n') {
        lines[last].push('\n');
    }
    lines.insert(last + 1, format!("{entry}\n"));
    Ok((format!("{head}{}", lines.concat()), 1))
}

/// The UTC calendar date `YYYY-MM-DD` of a Unix time in seconds.
pub fn date_of(unix: i64) -> String {
    // Days to civil date: the inverse of the rule in `guard::days_from_civil`.
    let days = unix.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::ProblemKind::*;
    use super::*;
    use crate::memory::home::HUMAN_SEED;

    fn p(line: usize, kind: ProblemKind) -> Problem {
        Problem { line, kind }
    }

    fn card(body: &str) -> String {
        format!("---\ndescription: d\nkind: person\n---\n{body}")
    }

    fn obs(body: &str) -> String {
        format!("---\ndescription: d\nkind: person\n---\n{body}")
    }

    fn entries(count: usize) -> String {
        (0..count)
            .map(|i| format!("- [2026-01-02] fact {i} <!-- n=1 -->\n"))
            .collect()
    }

    #[test]
    fn slug_rules() {
        let forty = "a".repeat(40);
        for slug in ["kim", "a-1", "0x", "human", forty.as_str()] {
            assert_eq!(check_slug(slug), Ok(()), "{slug}");
        }
        let forty_one = "a".repeat(41);
        for slug in [
            "",
            "Kim",
            "-a",
            "a_b",
            "a.b",
            "a/b",
            "한",
            forty_one.as_str(),
        ] {
            assert_eq!(check_slug(slug), Err(p(0, Slug)), "{slug}");
        }
    }

    #[test]
    fn classify_and_reserved() {
        assert_eq!(
            classify("people/kim/card.md"),
            Some(("kim", PersonFile::Card))
        );
        assert_eq!(
            classify("people/kim/observations.md"),
            Some(("kim", PersonFile::Observations))
        );
        for path in [
            "people/kim/c.md",
            "people/kim/x/card.md",
            "people/card.md",
            "notes/kim/card.md",
        ] {
            assert_eq!(classify(path), None, "{path}");
        }
        assert!(is_reserved("people/human/card.md"));
        assert!(is_reserved("people/human/x.md"));
        assert!(!is_reserved("people/humans/card.md"));
        assert!(!is_reserved("system/human.md"));
    }

    #[test]
    fn card_problems() {
        let attr = |n: usize| format!("ATTRIBUTE: {}\n", "x".repeat(n));
        let many = |n: usize| "ATTRIBUTE: a\n".repeat(n);
        let cases: Vec<(String, Vec<Problem>)> = vec![
            (
                card(
                    "IDENTITY: Kim\nATTRIBUTE: tall\nRELATIONSHIP: works with: lee\nINSTRUCTION: be brief\n\n",
                ),
                vec![],
            ),
            (card("Kim\n"), vec![p(5, Prefix)]),
            (card("IDENTITY: \n"), vec![p(5, Prefix)]),
            (card("RELATIONSHIP: friend\n"), vec![p(5, Relationship)]),
            (
                card("RELATIONSHIP: friend: Lee\n"),
                vec![p(5, Relationship)],
            ),
            (card("RELATIONSHIP: : lee\n"), vec![p(5, Relationship)]),
            (card(&attr(189)), vec![]),
            (card(&attr(190)), vec![p(5, TooLong)]),
            (card(&many(40)), vec![]),
            (card(&many(42)), vec![p(45, TooMany), p(46, TooMany)]),
            (
                "---\ndescription: d\n---\nIDENTITY: Kim\n".to_string(),
                vec![p(1, Kind)],
            ),
            (
                "---\ndescription: d\nkind: robot\n---\nIDENTITY: Kim\n".to_string(),
                vec![p(1, Kind)],
            ),
        ];
        for (text, want) in cases {
            assert_eq!(check(PersonFile::Card, &text), want, "{text:?}");
        }
    }

    #[test]
    fn observation_problems() {
        let long = |n: usize| {
            format!(
                "## Explicit\n- [2026-01-02] {} <!-- n=1 -->\n",
                "x".repeat(n)
            )
        };
        let mut cases: Vec<(String, Vec<Problem>)> = vec![
            (
                obs(
                    "## Explicit\n- [2026-01-02] likes tea <!-- n=1 -->\n\n- [2026-01-03] runs <!-- n=12 -->\n",
                ),
                vec![],
            ),
            (obs("- [2026-01-02] x <!-- n=1 -->\n"), vec![p(5, Section)]),
            (obs("## Deductive\n"), vec![p(5, Section)]),
            (obs(&long(172)), vec![]),
            (obs(&long(173)), vec![p(6, TooLong)]),
            (obs(&format!("## Explicit\n{}", entries(40))), vec![]),
            (
                obs(&format!("## Explicit\n{}", entries(41))),
                vec![p(46, TooMany)],
            ),
        ];
        for bad in [
            "- [2026-02-30] x <!-- n=1 -->",
            "- [2026-01-02] x <!-- n=0 -->",
            "- [2026-01-02] x <!-- n=01 -->",
            "- [2026-01-02]  <!-- n=1 -->",
            "- [2026-01-02] a --> b <!-- n=1 -->",
            "free text",
        ] {
            cases.push((obs(&format!("## Explicit\n{bad}\n")), vec![p(6, Entry)]));
        }
        for (text, want) in cases {
            assert_eq!(check(PersonFile::Observations, &text), want, "{text:?}");
        }
    }

    #[test]
    fn observe_appends_and_repeats() {
        let start = new_observations("kim", Some("Kim"));
        let (text1, n1) = observe(&start, "likes tea", "2026-01-02").unwrap();
        assert_eq!(n1, 1);
        assert!(text1.ends_with("## Explicit\n- [2026-01-02] likes tea <!-- n=1 -->\n"));
        let (text2, n2) = observe(&text1, "  Likes   TEA ", "2026-01-05").unwrap();
        assert_eq!(n2, 2);
        assert_eq!(
            text2,
            text1.replace(
                "- [2026-01-02] likes tea <!-- n=1 -->",
                "- [2026-01-05] likes tea <!-- n=2 -->"
            )
        );
        let (text3, n3) = observe(&text2, "runs", "2026-01-06").unwrap();
        assert_eq!(n3, 1);
        assert_eq!(text3, format!("{text2}- [2026-01-06] runs <!-- n=1 -->\n"));
        for text in [&text1, &text2, &text3] {
            assert_eq!(check(PersonFile::Observations, text), vec![], "{text:?}");
        }
    }

    #[test]
    fn observe_keeps_other_lines() {
        let start = obs(
            "## Explicit\n- [2026-01-02] likes tea <!-- n=1 -->\nnote: x\n- [2026-01-03] runs <!-- n=1 -->\n\n## Other\n- [2026-01-04] swims <!-- n=1 -->\n",
        );
        let runs = "- [2026-01-03] runs <!-- n=1 -->\n";

        let (text, n) = observe(&start, "reads", "2026-01-07").unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            text,
            obs(
                "## Explicit\n- [2026-01-02] likes tea <!-- n=1 -->\nnote: x\n- [2026-01-03] runs <!-- n=1 -->\n- [2026-01-07] reads <!-- n=1 -->\n\n## Other\n- [2026-01-04] swims <!-- n=1 -->\n"
            )
        );

        let (text, n) = observe(&start, "runs", "2026-01-08").unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            text,
            start.replace(runs, "- [2026-01-08] runs <!-- n=2 -->\n")
        );

        let (text, n) = observe(&start, "swims", "2026-01-09").unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            text,
            start.replace(runs, &format!("{runs}- [2026-01-09] swims <!-- n=1 -->\n"))
        );

        let twice = obs(
            "## Explicit\n- [2026-01-02] a <!-- n=1 -->\n## Explicit\n- [2026-01-03] b <!-- n=1 -->\n",
        );
        let (text, n) = observe(&twice, "c", "2026-01-10").unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            text,
            obs(
                "## Explicit\n- [2026-01-02] a <!-- n=1 -->\n- [2026-01-10] c <!-- n=1 -->\n## Explicit\n- [2026-01-03] b <!-- n=1 -->\n"
            )
        );
    }

    #[test]
    fn observe_limits() {
        let full = obs(&format!("## Explicit\n{}", entries(40)));
        assert_eq!(observe(&full, "new fact", "2026-01-05"), Err(p(0, TooMany)));
        let (_, n) = observe(&full, "fact 7", "2026-01-05").unwrap();
        assert_eq!(n, 2);

        let start = new_observations("kim", None);
        for fact in ["a\nb", "a <!-- b", "a --> b", "", "   "] {
            assert_eq!(
                observe(&start, fact, "2026-01-05"),
                Err(p(0, Entry)),
                "{fact:?}"
            );
        }
        let (_, n) = observe(&start, &"x".repeat(172), "2026-01-05").unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            observe(&start, &"x".repeat(173), "2026-01-05"),
            Err(p(0, TooLong))
        );
    }

    #[test]
    fn observe_into_the_owner_card() {
        let (text, n) = observe(HUMAN_SEED, "prefers short replies", "2026-01-02").unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            text,
            format!(
                "{HUMAN_SEED}\n## Explicit\n- [2026-01-02] prefers short replies <!-- n=1 -->\n"
            )
        );
        let head = |t: &str| {
            let parsed = frontmatter::parse(t).unwrap();
            t[..t.len() - parsed.body.len()].to_string()
        };
        assert_eq!(head(&text), head(HUMAN_SEED));
    }

    #[test]
    fn new_files_are_valid() {
        let text = new_card("kim", Some("Kim"), Some("Kimmy, K"));
        let parsed = frontmatter::parse(&text).unwrap();
        assert_eq!(parsed.frontmatter.get("kind"), Some("person"));
        assert_eq!(parsed.frontmatter.get("aliases"), Some("Kimmy, K"));
        assert_eq!(parsed.body, "IDENTITY: Kim\n");
        assert_eq!(check(PersonFile::Card, &text), vec![]);

        let text = new_card("kim", None, None);
        let parsed = frontmatter::parse(&text).unwrap();
        assert_eq!(parsed.body, "");
        assert_eq!(parsed.frontmatter.description(), "About kim");
        assert_eq!(parsed.frontmatter.get("aliases"), None);

        let text = new_observations("kim", Some("Kim"));
        assert_eq!(check(PersonFile::Observations, &text), vec![]);
    }

    #[test]
    fn date_of_known_instants() {
        for (unix, date) in [
            (0, "1970-01-01"),
            (951_782_400, "2000-02-29"),
            (1_767_225_600, "2026-01-01"),
            (1_767_225_599, "2025-12-31"),
            (4_107_542_400, "2100-03-01"),
        ] {
            assert_eq!(date_of(unix), date, "{unix}");
        }
    }
}
