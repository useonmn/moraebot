//! The persona file: what the owner chose at the first meeting, appended to the lead's system
//! prompt at every start.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::secret;

/// The persona file name under the moraebot home.
pub const FILE: &str = "persona.md";
/// The largest persona file, in bytes.
pub const MAX_BYTES: usize = 2048;
/// The line put above the persona text in the lead's system prompt.
pub const HEADER: &str = "[moraebot persona] What the owner chose at the first meeting: your name, how you talk and a few basics about them. The owner can change it with moraebot onboarding or by editing persona.md in the moraebot home; a change applies the next time moraebot starts.";
/// An empty name or a line break in a field.
pub const E_PERSONA_FIELD: &str = "E_PERSONA_FIELD";
/// More than `MAX_BYTES`.
pub const E_PERSONA_SIZE: &str = "E_PERSONA_SIZE";
/// Secret-like text.
pub const E_PERSONA_SECRET: &str = "E_PERSONA_SECRET";
/// The file was not written.
pub const E_PERSONA_WRITE: &str = "E_PERSONA_WRITE";
/// The file is not readable as UTF-8 text.
pub const E_PERSONA_READ: &str = "E_PERSONA_READ";

/// A coded refusal; `detail` never holds field or file text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaError {
    pub code: &'static str,
    pub detail: String,
}

impl PersonaError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    fn io(code: &'static str, err: &io::Error) -> Self {
        Self::new(code, format!("{:?}", err.kind()))
    }
}

/// `<home>/persona.md`.
pub fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

/// The file text: `Name: <name>\n`, then `Tone: <tone>\n` when `tone` is not empty, then
/// `Owner: <owner>\n` when `owner` is not empty; each field trimmed first.
pub fn render(name: &str, tone: &str, owner: &str) -> Result<String, PersonaError> {
    let fields = [
        ("name", name.trim()),
        ("tone", tone.trim()),
        ("owner", owner.trim()),
    ];
    if fields[0].1.is_empty() {
        return Err(PersonaError::new(E_PERSONA_FIELD, "name is empty"));
    }
    if let Some((field, _)) = fields
        .iter()
        .find(|(_, value)| value.contains(['\n', '\r']))
    {
        return Err(PersonaError::new(
            E_PERSONA_FIELD,
            format!("{field} has a line break"),
        ));
    }
    for (field, value) in fields {
        if let Some(found) = secret::find_secret(value) {
            return Err(PersonaError::new(
                E_PERSONA_SECRET,
                format!("{field}: secret-like text ({})", found.class.name()),
            ));
        }
    }
    let mut text = String::new();
    for (label, (_, value)) in ["Name", "Tone", "Owner"].into_iter().zip(fields) {
        if !value.is_empty() {
            text.push_str(&format!("{label}: {value}\n"));
        }
    }
    if text.len() > MAX_BYTES {
        return Err(PersonaError::new(
            E_PERSONA_SIZE,
            format!("{} bytes, limit {MAX_BYTES}", text.len()),
        ));
    }
    Ok(text)
}

/// Writes `text` to `<home>/.persona.md.tmp` (create+truncate, mode 0600, `sync_all`) and renames
/// it over `persona.md`. `E_PERSONA_WRITE` with the io error kind.
pub fn save(home: &Path, text: &str) -> Result<(), PersonaError> {
    let tmp = home.join(format!(".{FILE}.tmp"));
    let write = || -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path(home))
    };
    write().map_err(|e| PersonaError::io(E_PERSONA_WRITE, &e))
}

/// The trimmed file text; `Ok(None)` when the file is missing or only whitespace.
pub fn load(home: &Path) -> Result<Option<String>, PersonaError> {
    let path = path(home);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PersonaError::io(E_PERSONA_READ, &e)),
    };
    if !meta.is_file() {
        return Err(PersonaError::new(E_PERSONA_READ, "not a regular file"));
    }
    if meta.len() > MAX_BYTES as u64 {
        return Err(PersonaError::new(
            E_PERSONA_SIZE,
            format!("{} bytes, limit {MAX_BYTES}", meta.len()),
        ));
    }
    let bytes = fs::read(&path).map_err(|e| PersonaError::io(E_PERSONA_READ, &e))?;
    let text =
        String::from_utf8(bytes).map_err(|_| PersonaError::new(E_PERSONA_READ, "not UTF-8"))?;
    if let Some(found) = secret::find_secret(&text) {
        return Err(PersonaError::new(
            E_PERSONA_SECRET,
            format!("secret-like text ({})", found.class.name()),
        ));
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_owned()))
}

/// `hints`, then a blank line, `HEADER`, `\n` and `persona` (just `HEADER\npersona` when `hints`
/// is empty; `hints` unchanged when `persona` is `None`).
pub fn append(hints: &str, persona: Option<&str>) -> String {
    match persona {
        None => hints.to_owned(),
        Some(persona) if hints.is_empty() => format!("{HEADER}\n{persona}"),
        Some(persona) => format!("{hints}\n\n{HEADER}\n{persona}"),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn qa_home() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("qa-mbhome");
        fs::create_dir(&home).unwrap();
        (dir, home)
    }

    fn code<T: std::fmt::Debug>(result: Result<T, PersonaError>) -> &'static str {
        result.unwrap_err().code
    }

    #[test]
    fn render_writes_fixed_lines() {
        assert_eq!(
            render("Sora", "short and warm", "Owner 111 likes short replies").unwrap(),
            "Name: Sora\nTone: short and warm\nOwner: Owner 111 likes short replies\n"
        );
        assert_eq!(render(" Sora ", "", "").unwrap(), "Name: Sora\n");
    }

    #[test]
    fn render_refuses_bad_fields() {
        assert_eq!(code(render("  ", "t", "o")), E_PERSONA_FIELD);
        assert_eq!(code(render("So\nra", "", "")), E_PERSONA_FIELD);
        let owner = ["xo", "xb-"].concat() + &"1".repeat(10);
        let err = render("Sora", "", &owner).unwrap_err();
        assert_eq!(err.code, E_PERSONA_SECRET);
        assert!(!err.detail.contains(&owner));
    }

    #[test]
    fn render_size_limit_is_exact() {
        let text = render("X", "", &"a".repeat(2032)).unwrap();
        assert_eq!(text.len(), 2048);
        assert_eq!(code(render("X", "", &"a".repeat(2033))), E_PERSONA_SIZE);
    }

    #[test]
    fn save_then_load_round_trips() {
        let (_dir, home) = qa_home();
        let text = render("Sora", "short and warm", "Owner 111 likes short replies").unwrap();
        save(&home, &text).unwrap();
        let mode = fs::metadata(path(&home)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!home.join(".persona.md.tmp").exists());
        assert_eq!(load(&home), Ok(Some(text.trim_end().to_owned())));
    }

    #[test]
    fn load_missing_or_blank_is_none() {
        let (_dir, home) = qa_home();
        assert_eq!(load(&home), Ok(None));
        fs::write(path(&home), "  \n").unwrap();
        assert_eq!(load(&home), Ok(None));
    }

    #[test]
    fn load_refuses_odd_files() {
        let (_dir, home) = qa_home();
        fs::write(path(&home), "a".repeat(2049)).unwrap();
        assert_eq!(code(load(&home)), E_PERSONA_SIZE);
        let secret = ["gl", "pat-"].concat() + &"c".repeat(20);
        fs::write(path(&home), format!("Name: X\nOwner: {secret}")).unwrap();
        assert_eq!(code(load(&home)), E_PERSONA_SECRET);
        fs::write(path(&home), [0xff, 0xfe]).unwrap();
        assert_eq!(code(load(&home)), E_PERSONA_READ);
        fs::remove_file(path(&home)).unwrap();
        fs::create_dir(path(&home)).unwrap();
        assert_eq!(code(load(&home)), E_PERSONA_READ);
    }

    #[test]
    fn append_joins_hints_and_block() {
        assert_eq!(
            append("H", Some("Name: Sora")),
            format!("H\n\n{HEADER}\nName: Sora")
        );
        assert_eq!(
            append("", Some("Name: Sora")),
            format!("{HEADER}\nName: Sora")
        );
        assert_eq!(append("H", None), "H");
    }
}
