//! The first meeting with the owner: the request file, the state in the inbox database and the
//! lead's finish tool.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::home::MemoryError;
use super::{E_ONBOARDING_REQUEST, ONBOARDING_REQUEST_FILE};

/// `<home>/onboarding.request`, `home` being the moraebot home.
pub fn request_file(home: &Path) -> PathBuf {
    home.join(ONBOARDING_REQUEST_FILE)
}

/// Creates (or truncates) the request file, mode 0600. `E_ONBOARDING_REQUEST` with the
/// io error kind as detail when it cannot be written.
pub fn request(home: &Path) -> Result<(), MemoryError> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(request_file(home))
        .map(|_| ())
        .map_err(|e| MemoryError {
            code: E_ONBOARDING_REQUEST,
            detail: format!("{:?}", e.kind()),
        })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn request_file_is_under_the_home() {
        let home = Path::new("qa-mbhome");
        assert_eq!(request_file(home), home.join("onboarding.request"));
    }

    #[test]
    fn request_creates_a_private_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        request(dir.path()).expect("request");
        let meta = std::fs::metadata(request_file(dir.path())).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.len(), 0);
        request(dir.path()).expect("second request");
    }

    #[test]
    fn request_failure_is_coded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("plain-file");
        std::fs::write(&home, b"x").unwrap();
        let err = request(&home).expect_err("home is a file");
        assert_eq!(err.code, E_ONBOARDING_REQUEST);
    }
}
