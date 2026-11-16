//! Per-messenger behavior hints sent to omp.
//!
//! Each adapter has a built-in default. `<home>/hints/<platform>.md` overrides it: a
//! regular file of at most [`MAX_BYTES`] bytes of UTF-8 whose trimmed content is used;
//! an empty file means no hints.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::ConfigError;

/// The override file is not a regular file, too large, unreadable or not UTF-8.
pub const E_HINTS_FILE: &str = "E_HINTS_FILE";
/// Largest accepted override file.
pub const MAX_BYTES: u64 = 8192;

/// `<home>/hints/<platform>.md`.
pub fn override_path(home: &Path, platform: &str) -> PathBuf {
    home.join("hints").join(format!("{platform}.md"))
}

/// The hints for `platform`: the override file's trimmed content when it exists, else
/// `default`.
pub fn load(home: &Path, platform: &str, default: &str) -> Result<String, ConfigError> {
    let path = override_path(home, platform);
    let meta = match fs::metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(default.to_owned()),
        Err(err) => {
            return Err(ConfigError::io(
                E_HINTS_FILE,
                &format!("cannot read hints file {}", path.display()),
                &err,
            ));
        }
    };
    if !meta.is_file() {
        return Err(ConfigError::new(
            E_HINTS_FILE,
            format!("hints file {} is not a regular file", path.display()),
        ));
    }
    if meta.len() > MAX_BYTES {
        return Err(ConfigError::new(
            E_HINTS_FILE,
            format!(
                "hints file {} is larger than {MAX_BYTES} bytes",
                path.display()
            ),
        ));
    }
    let bytes = fs::read(&path).map_err(|err| {
        ConfigError::io(
            E_HINTS_FILE,
            &format!("cannot read hints file {}", path.display()),
            &err,
        )
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        ConfigError::new(
            E_HINTS_FILE,
            format!("hints file {} is not UTF-8", path.display()),
        )
    })?;
    Ok(text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = "built-in hints";

    fn home_with(content: &[u8]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("hints")).expect("hints dir");
        fs::write(override_path(dir.path(), "fake"), content).expect("write hints");
        dir
    }

    #[test]
    fn default_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(load(dir.path(), "fake", DEFAULT).unwrap(), DEFAULT);
    }

    #[test]
    fn override_used() {
        let dir = home_with(b"  custom hints\n");
        assert_eq!(load(dir.path(), "fake", DEFAULT).unwrap(), "custom hints");
    }

    #[test]
    fn empty_override_means_none() {
        let dir = home_with(b" \n\n");
        assert_eq!(load(dir.path(), "fake", DEFAULT).unwrap(), "");
    }

    #[test]
    fn oversize_override_rejected() {
        let dir = home_with(&vec![b'x'; MAX_BYTES as usize + 1]);
        assert_eq!(
            load(dir.path(), "fake", DEFAULT).unwrap_err().code,
            E_HINTS_FILE
        );
    }

    #[test]
    fn largest_allowed_override_loads() {
        let dir = home_with(&vec![b'x'; MAX_BYTES as usize]);
        assert_eq!(
            load(dir.path(), "fake", DEFAULT).unwrap().len(),
            MAX_BYTES as usize
        );
    }

    #[test]
    fn non_utf8_rejected() {
        let dir = home_with(&[0xff, 0xfe, 0x00]);
        assert_eq!(
            load(dir.path(), "fake", DEFAULT).unwrap_err().code,
            E_HINTS_FILE
        );
    }

    #[test]
    fn directory_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(override_path(dir.path(), "fake")).expect("dir in place of file");
        assert_eq!(
            load(dir.path(), "fake", DEFAULT).unwrap_err().code,
            E_HINTS_FILE
        );
    }
}
