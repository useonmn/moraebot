//! Frontmatter of memory files: a small, strict subset of YAML scalars.

use std::fmt;

use super::{KNOWN_KEYS, MAX_DESCRIPTION_CHARS};

/// The line that opens and closes a frontmatter block.
const FENCE: &str = "---";
/// A value starting with one of these would be a YAML collection, block scalar, anchor, alias,
/// tag, directive or reserved indicator, so it is refused unquoted.
const NOT_SCALAR_START: [char; 9] = ['[', '{', '|', '>', '&', '*', '!', '%', '@'];

/// The parsed frontmatter of one memory file: known keys with plain string values, in file order.
#[derive(Clone, Debug, PartialEq)]
pub struct Frontmatter {
    entries: Vec<(String, String)>,
}

impl Frontmatter {
    /// The value of `key`, if the file sets it.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The description. A successful parse always has one.
    pub fn description(&self) -> &str {
        self.get("description").unwrap_or("")
    }

    /// Every key and value in file order.
    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    /// A frontmatter holding only `description`. It is not validated here: writers parse the
    /// full text again.
    pub fn with_description(description: &str) -> Self {
        Self {
            entries: vec![("description".to_string(), description.to_string())],
        }
    }

    /// Set `key` to `value`: in place when the key exists, appended otherwise.
    pub fn set(&mut self, key: &str, value: &str) {
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value.to_string(),
            None => self.entries.push((key.to_string(), value.to_string())),
        }
    }
}

/// A successfully parsed memory file: its frontmatter and everything after it.
pub struct Parsed<'a> {
    pub frontmatter: Frontmatter,
    pub body: &'a str,
}

/// Why a file's frontmatter was refused, and on which line (1 = the opening `---`).
#[derive(Debug, PartialEq)]
pub struct FrontmatterError {
    pub line: usize,
    pub reason: FrontmatterReason,
}

/// The typed reason for a refused frontmatter block.
#[derive(Debug, PartialEq)]
pub enum FrontmatterReason {
    /// The text does not start with a `---` line.
    Missing,
    /// There is no closing `---` line.
    Unclosed,
    /// A block line is not `key: value`.
    BadLine,
    /// The key is not one of the known keys.
    UnknownKey,
    /// The key appears twice.
    DuplicateKey,
    /// The line is a comment.
    Comment,
    /// The value is a collection, block scalar or other non-scalar.
    NotScalar,
    /// A quoted value is not exactly one string literal.
    BadString,
    /// The description is missing or empty.
    NoDescription,
    /// The description is too long, has a newline, or contains a reserved tag.
    BadDescription,
}

impl FrontmatterReason {
    fn text(&self) -> &'static str {
        match self {
            Self::Missing => "the file must start with a --- line",
            Self::Unclosed => "the frontmatter block has no closing --- line",
            Self::BadLine => "expected a `key: value` line",
            Self::UnknownKey => "unknown frontmatter key",
            Self::DuplicateKey => "the key is set twice",
            Self::Comment => "comment lines are not allowed",
            Self::NotScalar => "the value must be a plain scalar",
            Self::BadString => "the quoted value is not one valid string",
            Self::NoDescription => "description is required and must not be empty",
            Self::BadDescription => "description must be one line of at most 1024 characters",
        }
    }
}

impl fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason.text())
    }
}

fn err(line: usize, reason: FrontmatterReason) -> FrontmatterError {
    FrontmatterError { line, reason }
}

/// Split `text` into its frontmatter and body.
pub fn parse(text: &str) -> Result<Parsed<'_>, FrontmatterError> {
    let Some(rest) = text.strip_prefix("---\n") else {
        return Err(err(1, FrontmatterReason::Missing));
    };
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut consumed = 0;
    for (index, raw) in rest.split_inclusive('\n').enumerate() {
        consumed += raw.len();
        let line_no = index + 2;
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        if line == FENCE {
            check_description(&entries, line_no)?;
            return Ok(Parsed {
                frontmatter: Frontmatter { entries },
                body: &rest[consumed..],
            });
        }
        entries.push(parse_line(line, line_no, &entries)?);
    }
    Err(err(1, FrontmatterReason::Unclosed))
}

fn parse_line(
    line: &str,
    line_no: usize,
    seen: &[(String, String)],
) -> Result<(String, String), FrontmatterError> {
    if line.starts_with('#') {
        return Err(err(line_no, FrontmatterReason::Comment));
    }
    let Some((key, value)) = line.split_once(':') else {
        return Err(err(line_no, FrontmatterReason::BadLine));
    };
    let key_ok = !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    if !key_ok || !(value.is_empty() || value.starts_with(' ')) {
        return Err(err(line_no, FrontmatterReason::BadLine));
    }
    if !KNOWN_KEYS.contains(&key) {
        return Err(err(line_no, FrontmatterReason::UnknownKey));
    }
    if seen.iter().any(|(k, _)| k == key) {
        return Err(err(line_no, FrontmatterReason::DuplicateKey));
    }
    let value = value.trim();
    let value = if value.starts_with('"') {
        serde_json::from_str::<String>(value)
            .map_err(|_| err(line_no, FrontmatterReason::BadString))?
    } else if value.starts_with(NOT_SCALAR_START) {
        return Err(err(line_no, FrontmatterReason::NotScalar));
    } else {
        value.to_string()
    };
    if key == "description" {
        check_description_value(&value, line_no)?;
    }
    Ok((key.to_string(), value))
}

fn check_description_value(value: &str, line_no: usize) -> Result<(), FrontmatterError> {
    if value.is_empty() {
        return Err(err(line_no, FrontmatterReason::NoDescription));
    }
    if value.contains('\n')
        || value.chars().count() > MAX_DESCRIPTION_CHARS
        || value.contains("<description>")
        || value.contains("<parameter>")
    {
        return Err(err(line_no, FrontmatterReason::BadDescription));
    }
    Ok(())
}

fn check_description(
    entries: &[(String, String)],
    closing_line: usize,
) -> Result<(), FrontmatterError> {
    if entries.iter().any(|(k, _)| k == "description") {
        Ok(())
    } else {
        Err(err(closing_line, FrontmatterReason::NoDescription))
    }
}

/// Write the canonical frontmatter block: keys in stored order, a value that plain text could not
/// carry back unchanged as a JSON string.
pub fn render_frontmatter(fm: &Frontmatter) -> String {
    let mut out = String::from("---\n");
    for (key, value) in &fm.entries {
        let needs_quotes = value.is_empty()
            || value != value.trim()
            || value.contains('\n')
            || value.starts_with('"')
            || value.starts_with(NOT_SCALAR_START);
        out.push_str(key);
        out.push_str(": ");
        if needs_quotes {
            out.push_str(&serde_json::to_string(value).unwrap_or_default());
        } else {
            out.push_str(value);
        }
        out.push('\n');
    }
    out.push_str("---\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(text: &str) -> (usize, FrontmatterReason) {
        match parse(text) {
            Ok(_) => panic!("expected a refusal for {text:?}"),
            Err(e) => (e.line, e.reason),
        }
    }

    #[test]
    fn parses_description_and_body() {
        let parsed = parse("---\ndescription: d\n---\nbody\n").unwrap();
        assert_eq!(parsed.frontmatter.description(), "d");
        assert_eq!(parsed.body, "body\n");
    }

    #[test]
    fn known_keys_are_kept_in_order() {
        let text = "---\nexpires: 2030-01-01\nlimit: 500\ndescription: d\nkind: person\n\
                    read_only: true\naliases: a, b\nprovenance: user\n---\n";
        let parsed = parse(text).unwrap();
        let keys: Vec<&str> = parsed
            .frontmatter
            .entries()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "expires",
                "limit",
                "description",
                "kind",
                "read_only",
                "aliases",
                "provenance"
            ]
        );
        assert_eq!(parsed.frontmatter.get("aliases"), Some("a, b"));
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn quoted_values_are_decoded() {
        let parsed = parse("---\ndescription: \"a: b\"\n---\n").unwrap();
        assert_eq!(parsed.frontmatter.description(), "a: b");
        assert_eq!(
            refusal("---\ndescription: \"bad\n---\n"),
            (2, FrontmatterReason::BadString)
        );
    }

    #[test]
    fn refusals_have_line_and_reason() {
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        let cases = [
            (
                "no frontmatter\n".to_string(),
                1,
                FrontmatterReason::Missing,
            ),
            (String::new(), 1, FrontmatterReason::Missing),
            (
                "---\ndescription: d\n".to_string(),
                1,
                FrontmatterReason::Unclosed,
            ),
            (
                "---\ndescription: d\nnot a pair\n---\n".to_string(),
                3,
                FrontmatterReason::BadLine,
            ),
            (
                "---\ndescription: d\nfoo: x\n---\n".to_string(),
                3,
                FrontmatterReason::UnknownKey,
            ),
            (
                "---\ndescription: d\ndescription: e\n---\n".to_string(),
                3,
                FrontmatterReason::DuplicateKey,
            ),
            (
                "---\ndescription: d\n# note\n---\n".to_string(),
                3,
                FrontmatterReason::Comment,
            ),
            (
                "---\ndescription: d\naliases: [a]\n---\n".to_string(),
                3,
                FrontmatterReason::NotScalar,
            ),
            (
                "---\ndescription: |\n---\n".to_string(),
                2,
                FrontmatterReason::NotScalar,
            ),
            (
                "---\nkind: x\n---\n".to_string(),
                3,
                FrontmatterReason::NoDescription,
            ),
            (
                "---\ndescription:\n---\n".to_string(),
                2,
                FrontmatterReason::NoDescription,
            ),
            (
                format!("---\ndescription: {long}\n---\n"),
                2,
                FrontmatterReason::BadDescription,
            ),
            (
                "---\ndescription: a <parameter> b\n---\n".to_string(),
                2,
                FrontmatterReason::BadDescription,
            ),
        ];
        for (text, line, reason) in cases {
            assert_eq!(refusal(&text), (line, reason), "{text:?}");
        }
        let edge = "x".repeat(MAX_DESCRIPTION_CHARS);
        assert!(parse(&format!("---\ndescription: {edge}\n---\n")).is_ok());
    }

    #[test]
    fn canonical_block_roundtrips() {
        let texts = [
            "---\ndescription: plain\n---\nbody\n",
            "---\ndescription: \"  padded  \"\nkind: person\n---\n",
            "---\ndescription: d\naliases: \"[a, b]\"\nlimit: \"\"\n---\n\nbody\n",
            "---\ndescription: \"say \\\"hi\\\"\"\nprovenance: \"@owner\"\nexpires: 2030-01-01\n---\nx\n",
        ];
        for text in texts {
            let parsed = parse(text).unwrap();
            let rendered = format!("{}{}", render_frontmatter(&parsed.frontmatter), parsed.body);
            let again = parse(&rendered).unwrap();
            assert_eq!(again.frontmatter, parsed.frontmatter);
            assert_eq!(again.body, parsed.body);
        }
    }

    #[test]
    fn set_replaces_in_place_and_appends_new_keys() {
        let mut fm = parse("---\nkind: person\ndescription: d\nlimit: 5\n---\n")
            .unwrap()
            .frontmatter;
        fm.set("description", "x");
        let pairs = |fm: &Frontmatter| -> Vec<(String, String)> { fm.entries().to_vec() };
        let owned = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            pairs(&fm),
            [
                owned("kind", "person"),
                owned("description", "x"),
                owned("limit", "5")
            ]
        );
        fm.set("expires", "2027-01-01");
        assert_eq!(
            pairs(&fm),
            [
                owned("kind", "person"),
                owned("description", "x"),
                owned("limit", "5"),
                owned("expires", "2027-01-01")
            ]
        );
    }

    #[test]
    fn with_description_renders_and_parses() {
        let text = format!(
            "{}body\n",
            render_frontmatter(&Frontmatter::with_description("a: b"))
        );
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.frontmatter.description(), "a: b");
        assert_eq!(parsed.body, "body\n");
    }

    #[test]
    fn crlf_is_refused_as_bad_line() {
        assert_eq!(
            refusal("---\r\ndescription: d\r\n---\r\n"),
            (1, FrontmatterReason::Missing)
        );
    }
}
