//! Memory guards: `read_only`, `expires` and the secret table.

use std::time::{SystemTime, UNIX_EPOCH};

use super::home::MemoryError;
use super::{E_MEMORY_SECRET, EXPIRES_KEY, READ_ONLY_KEY, frontmatter};

/// Whether the file's frontmatter parses and sets `read_only` to exactly `true`.
pub fn is_read_only(text: &str) -> bool {
    frontmatter::parse(text)
        .is_ok_and(|parsed| parsed.frontmatter.get(READ_ONLY_KEY) == Some("true"))
}

/// The instant an `expires` value names, in Unix seconds, or `None` when it is unreadable.
/// Accepted: `YYYY-MM-DD` (that day at `00:00:00Z`) and
/// `YYYY-MM-DDTHH:MM:SS[.d+](Z|+HH:MM|-HH:MM)`.
pub fn expires_at(value: &str) -> Option<i64> {
    let b = value.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (year, month, day) = (number(&b[0..4])?, number(&b[5..7])?, number(&b[8..10])?);
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
    {
        return None;
    }
    let date = days_from_civil(year, month, day) * 86_400;
    let rest = &b[10..];
    if rest.is_empty() {
        return Some(date);
    }
    if rest.len() < 9 || rest[0] != b'T' || rest[3] != b':' || rest[6] != b':' {
        return None;
    }
    let (hour, minute, second) = (
        number(&rest[1..3])?,
        number(&rest[4..6])?,
        number(&rest[7..9])?,
    );
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut zone = &rest[9..];
    if let Some(fraction) = zone.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        zone = &fraction[digits..];
    }
    let offset = match zone {
        b"Z" => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let (oh, om) = (number(&[*h1, *h2])?, number(&[*m1, *m2])?);
            if oh > 23 || om > 59 {
                return None;
            }
            let offset = oh * 3600 + om * 60;
            if *sign == b'+' { offset } else { -offset }
        }
        _ => return None,
    };
    Some(date + hour * 3600 + minute * 60 + second - offset)
}

/// Whether the file's frontmatter parses and its readable `expires` is at or before `now`.
pub fn is_expired(text: &str, now: i64) -> bool {
    frontmatter::parse(text).is_ok_and(|parsed| {
        parsed
            .frontmatter
            .get(EXPIRES_KEY)
            .and_then(expires_at)
            .is_some_and(|at| now >= at)
    })
}

/// The wall clock in whole Unix seconds; a clock before 1970 gives `0`.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn number(digits: &[u8]) -> Option<i64> {
    digits.iter().try_fold(0i64, |n, c| {
        c.is_ascii_digit().then(|| n * 10 + i64::from(c - b'0'))
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a valid Gregorian date (the civil-from-days algorithm, inverted).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The kinds of secret the memory refuses to store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretClass {
    AwsAccessKey,
    PrivateKey,
    BearerToken,
    SkKey,
    GithubToken,
    GitlabToken,
    SlackToken,
    Assignment,
}

impl SecretClass {
    /// The stable name used in refusal details.
    pub fn name(self) -> &'static str {
        match self {
            Self::AwsAccessKey => "aws_access_key",
            Self::PrivateKey => "private_key",
            Self::BearerToken => "bearer_token",
            Self::SkKey => "sk_key",
            Self::GithubToken => "github_token",
            Self::GitlabToken => "gitlab_token",
            Self::SlackToken => "slack_token",
            Self::Assignment => "secret_assignment",
        }
    }
}

/// One secret found in a text: its class and byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecretMatch {
    pub class: SecretClass,
    pub start: usize,
    pub end: usize,
}

/// The secret table, in tie-break order.
const TABLE: [SecretClass; 8] = [
    SecretClass::AwsAccessKey,
    SecretClass::PrivateKey,
    SecretClass::BearerToken,
    SecretClass::SkKey,
    SecretClass::GithubToken,
    SecretClass::GitlabToken,
    SecretClass::SlackToken,
    SecretClass::Assignment,
];

/// Identifier names whose assigned value is treated as a secret.
const SECRET_NAMES: [&str; 7] = [
    "token",
    "api_key",
    "apikey",
    "password",
    "passwd",
    "secret",
    "private_key",
];

const GITHUB_PREFIXES: [&[u8]; 5] = [b"ghp_", b"gho_", b"ghu_", b"ghs_", b"ghr_"];
const PRIVATE_KEY_TAIL: &[u8] = b"PRIVATE KEY-----";

/// The secret with the smallest start in `text`; on a tie, the first class in table order.
pub fn find_secret(text: &str) -> Option<SecretMatch> {
    let b = text.as_bytes();
    (0..b.len()).find_map(|start| {
        TABLE.iter().find_map(|&class| {
            match_at(b, start, class).map(|end| SecretMatch { class, start, end })
        })
    })
}

/// `E_MEMORY_SECRET` naming `field` and the class when `value` holds a secret. The detail never
/// holds any byte of `value`.
pub fn refuse_secret(field: &str, value: &str) -> Result<(), MemoryError> {
    match find_secret(value) {
        Some(found) => Err(MemoryError {
            code: E_MEMORY_SECRET,
            detail: format!("{field}: secret-like content ({})", found.class.name()),
        }),
        None => Ok(()),
    }
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_ident(c: u8) -> bool {
    is_word(c) || c == b'-'
}

/// The start of the text, or a preceding byte that is not `[A-Za-z0-9_]`.
fn boundary(b: &[u8], i: usize) -> bool {
    i == 0 || !is_word(b[i - 1])
}

fn run(b: &[u8], from: usize, set: impl Fn(u8) -> bool) -> usize {
    from + b[from..].iter().take_while(|&&c| set(c)).count()
}

fn starts_ci(b: &[u8], i: usize, pat: &[u8]) -> bool {
    b.len() - i >= pat.len() && b[i..i + pat.len()].eq_ignore_ascii_case(pat)
}

fn long_run(b: &[u8], from: usize, min: usize, set: impl Fn(u8) -> bool) -> Option<usize> {
    let end = run(b, from, set);
    (end - from >= min).then_some(end)
}

fn match_at(b: &[u8], i: usize, class: SecretClass) -> Option<usize> {
    let rest = &b[i..];
    match class {
        SecretClass::AwsAccessKey => {
            if !boundary(b, i) || !(rest.starts_with(b"AKIA") || rest.starts_with(b"ASIA")) {
                return None;
            }
            let end = run(b, i + 4, |c| c.is_ascii_uppercase() || c.is_ascii_digit());
            let closed = end == b.len() || !b[end].is_ascii_alphanumeric();
            (end - (i + 4) == 16 && closed).then_some(end)
        }
        SecretClass::PrivateKey => {
            if !rest.starts_with(b"-----BEGIN ") {
                return None;
            }
            let first = i + 11;
            let mut q = first;
            loop {
                if b[q..].starts_with(PRIVATE_KEY_TAIL) {
                    return Some(q + PRIVATE_KEY_TAIL.len());
                }
                if q - first == 40 || q == b.len() || !(b[q].is_ascii_uppercase() || b[q] == b' ') {
                    return None;
                }
                q += 1;
            }
        }
        SecretClass::BearerToken => {
            if !starts_ci(b, i, b"authorization:") {
                return None;
            }
            let q = run(b, i + 14, |c| c == b' ' || c == b'\t');
            if !starts_ci(b, q, b"bearer") {
                return None;
            }
            let token = run(b, q + 6, |c| c == b' ');
            if token == q + 6 {
                return None;
            }
            long_run(b, token, 16, |c| {
                c.is_ascii_alphanumeric() || b"._~+/=-".contains(&c)
            })
        }
        SecretClass::SkKey => {
            if !boundary(b, i) || !rest.starts_with(b"sk-") {
                return None;
            }
            long_run(b, i + 3, 20, is_ident)
        }
        SecretClass::GithubToken => {
            if !boundary(b, i) {
                return None;
            }
            if GITHUB_PREFIXES.iter().any(|p| rest.starts_with(p)) {
                long_run(b, i + 4, 36, |c| c.is_ascii_alphanumeric())
            } else if rest.starts_with(b"github_pat_") {
                long_run(b, i + 11, 22, is_word)
            } else {
                None
            }
        }
        SecretClass::GitlabToken => {
            if !boundary(b, i) || !rest.starts_with(b"glpat-") {
                return None;
            }
            long_run(b, i + 6, 20, is_ident)
        }
        SecretClass::SlackToken => {
            if !boundary(b, i) {
                return None;
            }
            let slack_set = |c: u8| c.is_ascii_alphanumeric() || c == b'-';
            match rest {
                [b'x', b'o', b'x', b'a' | b'b' | b'p' | b'r' | b's', b'-', ..] => {
                    long_run(b, i + 5, 10, slack_set)
                }
                _ if rest.starts_with(b"xapp-") => long_run(b, i + 5, 10, slack_set),
                _ => None,
            }
        }
        SecretClass::Assignment => {
            // The identifier is the longest run before `=`, so it starts after a non-identifier
            // byte (which is always a boundary).
            if (i > 0 && is_ident(b[i - 1])) || !is_ident(b[i]) {
                return None;
            }
            let eq = run(b, i, is_ident);
            if eq == b.len() || b[eq] != b'=' || !secret_name(&b[i..eq]) {
                return None;
            }
            let mut value = eq + 1;
            if value < b.len() && (b[value] == b'"' || b[value] == b'\'') {
                value += 1;
            }
            let end = run(b, value, |c| {
                !(c.is_ascii_whitespace() || matches!(c, b'"' | b'\'' | b'<' | b'>'))
            });
            (end - value >= 8 && !matches!(b[value], b'$' | b'{' | b'*')).then_some(end)
        }
    }
}

/// Whether the identifier, ASCII lower-cased, equals one of `SECRET_NAMES` or ends with `_` or
/// `-` followed by one.
fn secret_name(ident: &[u8]) -> bool {
    SECRET_NAMES.iter().any(|name| {
        let name = name.as_bytes();
        let n = ident.len();
        if n == name.len() {
            ident.eq_ignore_ascii_case(name)
        } else {
            n > name.len()
                && ident[n - name.len()..].eq_ignore_ascii_case(name)
                && matches!(ident[n - name.len() - 1], b'_' | b'-')
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parts` joined, then `n` copies of `fill`: token-shaped values exist only at run time.
    fn fake(parts: &[&str], fill: char, n: usize) -> String {
        let mut s = parts.concat();
        s.extend(std::iter::repeat_n(fill, n));
        s
    }

    fn with_frontmatter(lines: &str) -> String {
        format!("---\ndescription: guard test\n{lines}---\nbody\n")
    }

    #[test]
    fn secret_table_finds_each_class() {
        let header = |kind: &str| ["-----BEGIN ", kind, "PRIVATE KEY", "-----"].concat();
        let cases: Vec<(String, SecretClass)> = vec![
            (fake(&["AK", "IA"], 'Q', 16), SecretClass::AwsAccessKey),
            (fake(&["AS", "IA"], '7', 16), SecretClass::AwsAccessKey),
            (header("OPENSSH "), SecretClass::PrivateKey),
            (header(""), SecretClass::PrivateKey),
            (
                fake(
                    &["aUTHORIZATION:", " \t", "BeArEr  ", "ab.c_d~e+f/="],
                    'z',
                    6,
                ),
                SecretClass::BearerToken,
            ),
            (fake(&["s", "k-", "proj-"], 'k', 20), SecretClass::SkKey),
            (fake(&["gh", "p_"], 'A', 36), SecretClass::GithubToken),
            (
                fake(&["github", "_pat_"], 'B', 22),
                SecretClass::GithubToken,
            ),
            (fake(&["gl", "pat-"], 'c', 20), SecretClass::GitlabToken),
            (fake(&["xo", "xb-"], '1', 10), SecretClass::SlackToken),
            (fake(&["xa", "pp-"], 'C', 10), SecretClass::SlackToken),
            (
                fake(&["GITHUB_", "TOKEN=", "v4lue"], 'x', 8),
                SecretClass::Assignment,
            ),
            (
                fake(&["pass", "word=\"", "horse"], 'q', 8),
                SecretClass::Assignment,
            ),
        ];
        let prefix = "메모 내용: ";
        for (index, (secret, class)) in cases.iter().enumerate() {
            let text = format!("{prefix}{secret}\" 다음 줄\n");
            let start = prefix.len();
            assert_eq!(
                find_secret(&text),
                Some(SecretMatch {
                    class: *class,
                    start,
                    end: start + secret.len(),
                }),
                "case {index} ({})",
                class.name()
            );
        }
    }

    #[test]
    fn secret_look_alikes_pass() {
        let cases: Vec<String> = vec![
            "ask-for-the-quarterly-report-today".into(),
            "risk-free-option-for-the-whole-team".into(),
            "-----BEGIN PUBLIC KEY-----".into(),
            "-----BEGIN CERTIFICATE-----".into(),
            fake(&["AK", "IA"], 'Q', 15),
            fake(&["AK", "IA"], 'Q', 17),
            "Bearer of bad news".into(),
            "Authorization: Bearer <token>".into(),
            "password=<hidden>".into(),
            "token=${API_TOKEN}".into(),
            "password=********".into(),
            "token=short".into(),
            "max_tokens=4096".into(),
            fake(&["gh", "p_"], 'A', 35),
            fake(&["gl", "pat-"], 'c', 19),
            fake(&["xo", "xb-"], '1', 9),
            "the password reset happened".into(),
        ];
        for (index, text) in cases.iter().enumerate() {
            assert_eq!(find_secret(text), None, "case {index}");
        }
    }

    #[test]
    fn earliest_secret_wins() {
        let header = ["-----BEGIN RSA ", "PRIVATE KEY", "-----"].concat();
        let sk = fake(&["s", "k-"], 'k', 24);
        let text = format!("키: {header} 그리고 {sk}");
        let start = "키: ".len();
        assert_eq!(
            find_secret(&text),
            Some(SecretMatch {
                class: SecretClass::PrivateKey,
                start,
                end: start + header.len(),
            })
        );

        // `sk_key` and `secret_assignment` both start at the `s`; the table order picks `sk_key`.
        let both = fake(&["s", "k-"], 'a', 20) + "_token=" + &"v".repeat(8);
        let key_end = both.find('=').unwrap();
        assert_eq!(
            find_secret(&format!(" {both}")),
            Some(SecretMatch {
                class: SecretClass::SkKey,
                start: 1,
                end: 1 + key_end,
            })
        );
        assert!(match_at(both.as_bytes(), 0, SecretClass::Assignment).is_some());
    }

    #[test]
    fn refusal_names_class_not_text() {
        let s = format!("note: {}", fake(&["gl", "pat-"], 'c', 24));
        let m = find_secret(&s).expect("a secret");
        let e = refuse_secret("text", &s).expect_err("refused");
        assert_eq!(e.code, E_MEMORY_SECRET);
        assert!(!e.detail.contains(&s[m.start..m.end]));
        assert!(e.detail.contains(m.class.name()));
        assert!(e.detail.starts_with("text: "));
        assert!(refuse_secret("path", "notes/plain.md").is_ok());
    }

    #[test]
    fn read_only_only_for_true() {
        assert!(is_read_only(&with_frontmatter("read_only: \"true\"\n")));
        assert!(is_read_only(&with_frontmatter("read_only: true\n")));
        for value in ["\"false\"", "yes", "True"] {
            assert!(
                !is_read_only(&with_frontmatter(&format!("read_only: {value}\n"))),
                "{value}"
            );
        }
        assert!(!is_read_only(&with_frontmatter("")));
        assert!(!is_read_only("read_only: true\nno frontmatter\n"));
    }

    #[test]
    fn expires_forms() {
        assert_eq!(expires_at("2026-11-17"), Some(1_794_873_600));
        for value in [
            "2026-11-17T09:30:00Z",
            "2026-11-17T18:30:00+09:00",
            "2026-11-17T04:30:00-05:00",
            "2026-11-17T09:30:00.250Z",
        ] {
            assert_eq!(expires_at(value), Some(1_794_907_800), "{value}");
        }
        assert_eq!(expires_at("2024-02-29"), Some(1_709_164_800));
        assert_eq!(expires_at("1970-01-01"), Some(0));
        for value in [
            "2025-02-29",
            "2026-13-01",
            "2026-11-31",
            "2026-11-17T09:30:00",
            "2026-11-17 09:30:00Z",
            "2026-11-17t09:30:00z",
            "2026-11-17T24:00:00Z",
            "2026-11-17T09:30:60Z",
            "2026-11-17T09:30:00+24:00",
            "2026-11-17x",
            "tomorrow",
            "",
        ] {
            assert_eq!(expires_at(value), None, "{value}");
        }
    }

    #[test]
    fn expired_from_the_instant() {
        let dated = with_frontmatter("expires: 2026-11-17\n");
        assert!(!is_expired(&dated, 1_794_873_599));
        assert!(is_expired(&dated, 1_794_873_600));
        assert!(is_expired(&dated, i64::MAX));
        for text in [
            with_frontmatter("expires: soon\n"),
            with_frontmatter(""),
            "expires: 2000-01-01\nno frontmatter\n".to_string(),
        ] {
            assert!(!is_expired(&text, 0));
            assert!(!is_expired(&text, i64::MAX));
        }
    }
}
