//! Lane work folders: a lane started with a `repo` works in its own git worktree of that checkout
//! (moraebot creates it; Dori mode lets the lane create it). Messenger-agnostic.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Folder in the work folder that holds the lanes' worktrees.
pub const LANES_DIR: &str = ".moraebot-lanes";
/// A lane's branch is this plus its id.
pub const BRANCH_PREFIX: &str = "moraebot/lane-";
/// Kill deadline of one git command (Dori mode command limit).
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// git could not run or passed `GIT_TIMEOUT`.
pub const E_LANE_GIT: &str = "E_LANE_GIT";
/// `repo` is not an existing folder, or not inside a git checkout.
pub const E_LANE_REPO: &str = "E_LANE_REPO";
/// The lane's worktree could not be created.
pub const E_LANE_WORKTREE: &str = "E_LANE_WORKTREE";

/// `<work_dir>/.moraebot-lanes/lane-<lane_id>`.
pub fn folder(work_dir: &Path, lane_id: i64) -> PathBuf {
    work_dir.join(LANES_DIR).join(format!("lane-{lane_id}"))
}

/// `moraebot/lane-<lane_id>`.
pub fn branch(lane_id: i64) -> String {
    format!("{BRANCH_PREFIX}{lane_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_and_branch_names() {
        assert_eq!(
            folder(Path::new("/w"), 7),
            PathBuf::from("/w/.moraebot-lanes/lane-7")
        );
        assert_eq!(branch(7), "moraebot/lane-7");
    }
}
