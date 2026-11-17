//! What changed since the lead's block was pinned, and the notice lines that say so.

use std::path::{Path, PathBuf};

use super::{MEMORY_DIR, REFRESH_FILE};

/// `<home>/memory/.refresh`: the explicit refresh request, `home` being the moraebot home.
pub fn refresh_file(home: &Path) -> PathBuf {
    home.join(MEMORY_DIR).join(REFRESH_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_file_is_under_the_memory_root() {
        let home = Path::new("qa-mbhome");
        assert_eq!(refresh_file(home), home.join("memory").join(".refresh"));
    }
}
