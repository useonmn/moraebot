//! Lane work folders: a lane started with a `repo` works in its own git worktree of that checkout
//! (moraebot creates it; Dori mode lets the lane create it). Messenger-agnostic.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::done::{self, Check, Ran, RunOpts, Stopped};
use crate::omp;

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
/// A lane worktree has changes to tracked files that are not committed.
pub const E_CHECK_UNCOMMITTED: &str = "E_CHECK_UNCOMMITTED";
/// A lane worktree's HEAD has commits that are on no remote.
pub const E_CHECK_UNPUSHED: &str = "E_CHECK_UNPUSHED";
/// A lane worktree's git state could not be read.
pub const E_CHECK_GIT: &str = "E_CHECK_GIT";
/// Appended after the Done footer of a lane with a repo.
pub const GIT_FOOTER: &str = "\n[moraebot] This lane works in its own git worktree. Before lane_done, commit your changes to tracked files and push your branch (for example git push origin HEAD) if the repo has a remote. moraebot refuses the claim while tracked changes are uncommitted or commits are on no remote; new untracked files do not count.";

/// `<work_dir>/.moraebot-lanes/lane-<lane_id>`.
pub fn folder(work_dir: &Path, lane_id: i64) -> PathBuf {
    work_dir.join(LANES_DIR).join(format!("lane-{lane_id}"))
}

/// `moraebot/lane-<lane_id>`.
pub fn branch(lane_id: i64) -> String {
    format!("{BRANCH_PREFIX}{lane_id}")
}

/// Empty file written into a lane worktree's own git dir only after a complete `worktree add`:
/// the stored fact that the folder is a finished worktree made by moraebot.
const READY_MARKER: &str = "moraebot-ready";

/// Why a lane's worktree is not ready.
#[derive(Debug)]
pub(crate) enum WorktreeError {
    /// A coded failure; `detail` is short and never logged with paths.
    Failed { code: &'static str, detail: String },
    /// moraebot started stopping before a git command could run.
    Stopped,
}

fn failed(code: &'static str, detail: impl Into<String>) -> WorktreeError {
    WorktreeError::Failed {
        code,
        detail: detail.into(),
    }
}

/// The environment of moraebot's own git commands: never prompt, never take an optional lock
/// (status stays read-only).
fn git_env(env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    let mut env = env.to_vec();
    env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    env.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
    env
}

fn args(parts: &[&OsStr]) -> Vec<OsString> {
    parts.iter().map(|part| part.to_os_string()).collect()
}

/// The trimmed lines of the last command's stdout in `out_dir`.
fn stdout_lines(out_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(out_dir.join("stdout"))
        .unwrap_or_default()
        .lines()
        .map(|line| line.trim().to_owned())
        .collect()
}

/// The detail of a command that did not exit 0: the head of its output.
fn output_detail(out_dir: &Path) -> String {
    done::short(&done::output_head(out_dir))
}

/// The detail of a git command that could not run.
fn git_detail(ran: &Ran, opts: &RunOpts<'_>) -> String {
    match ran {
        Ran::Spawn(detail) | Ran::WaitError(detail) => done::short(detail),
        Ran::Timeout => format!("killed after {} s", opts.timeout.as_secs()),
        Ran::Exited(_) | Ran::Signalled => output_detail(opts.out_dir),
    }
}

/// Makes sure `folder` is lane `lane_id`'s finished git worktree of the checkout `repo`: an
/// existing path is only checked with read-only git, never changed or removed; a missing one
/// is created with `git worktree add -b moraebot/lane-<id> <folder> HEAD` and marked ready.
/// `started`/`ended` see every git pid as in [`done::run_command`].
pub(crate) fn ensure(
    repo: &Path,
    folder: &Path,
    lane_id: i64,
    opts: &RunOpts<'_>,
    mut started: impl FnMut(u32) -> bool,
    mut ended: impl FnMut(u32),
) -> Result<(), WorktreeError> {
    let env = git_env(opts.env);
    let opts = RunOpts {
        out_dir: opts.out_dir,
        env: &env,
        timeout: opts.timeout,
    };
    let git = OsStr::new("git");
    let stopped = |_| WorktreeError::Stopped;

    // 1. git can run (the macOS xcrun stub starts but exits 1).
    let ran = done::run_command(
        git,
        &args(&[OsStr::new("--version")]),
        repo,
        &opts,
        &mut started,
        &mut ended,
    )
    .map_err(stopped)?;
    if !matches!(ran, Ran::Exited(0)) {
        return Err(failed(E_LANE_GIT, git_detail(&ran, &opts)));
    }

    // 2. The checkout's top level and common git dir.
    let ran = done::run_command(
        git,
        &args(&[
            OsStr::new("-C"),
            repo.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--show-toplevel"),
            OsStr::new("--git-common-dir"),
        ]),
        repo,
        &opts,
        &mut started,
        &mut ended,
    )
    .map_err(stopped)?;
    match ran {
        Ran::Spawn(_) | Ran::Timeout => return Err(failed(E_LANE_GIT, git_detail(&ran, &opts))),
        Ran::Exited(0) => {}
        _ => return Err(failed(E_LANE_REPO, output_detail(opts.out_dir))),
    }
    let lines = stdout_lines(opts.out_dir);
    let (Some(top), Some(common)) = (lines.first(), lines.get(1)) else {
        return Err(failed(E_LANE_REPO, output_detail(opts.out_dir)));
    };
    let top = PathBuf::from(top);
    let Ok(common) = std::fs::canonicalize(repo.join(common)) else {
        return Err(failed(E_LANE_REPO, output_detail(opts.out_dir)));
    };

    // 3. An existing path is reused only when it is this lane's finished worktree.
    if let Ok(meta) = folder.symlink_metadata() {
        if !meta.is_dir() {
            return Err(failed(E_LANE_WORKTREE, "lane folder is not a directory"));
        }
        return verify(
            folder,
            lane_id,
            &common,
            &opts,
            true,
            &mut started,
            &mut ended,
        )
        .map(|_| ());
    }

    // 4. The folder that holds the lanes' worktrees.
    let made = match folder.parent() {
        Some(parent) => omp::private_dir(parent),
        None => Err(std::io::ErrorKind::NotFound.into()),
    };
    if let Err(error) = made {
        return Err(failed(E_LANE_WORKTREE, done::short(&error.to_string())));
    }

    // 5. The worktree on the lane's own branch, from the checkout's HEAD.
    let branch = branch(lane_id);
    let ran = done::run_command(
        git,
        &args(&[
            OsStr::new("-C"),
            top.as_os_str(),
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(&branch),
            folder.as_os_str(),
            OsStr::new("HEAD"),
        ]),
        &top,
        &opts,
        &mut started,
        &mut ended,
    )
    .map_err(stopped)?;
    match ran {
        Ran::Spawn(_) | Ran::Timeout => return Err(failed(E_LANE_GIT, git_detail(&ran, &opts))),
        Ran::Exited(0) => {}
        _ => return Err(failed(E_LANE_WORKTREE, output_detail(opts.out_dir))),
    }

    // 6. Checked like a reused folder, then marked finished.
    let git_dir = verify(
        folder,
        lane_id,
        &common,
        &opts,
        false,
        &mut started,
        &mut ended,
    )?;
    std::fs::write(git_dir.join(READY_MARKER), b"")
        .map_err(|error| failed(E_LANE_WORKTREE, done::short(&error.to_string())))
}

/// Read-only check that `folder` is lane `lane_id`'s worktree of the checkout whose common git
/// dir is `common` (and, with `need_marker`, a finished one); returns the worktree's git dir.
fn verify(
    folder: &Path,
    lane_id: i64,
    common: &Path,
    opts: &RunOpts<'_>,
    need_marker: bool,
    started: &mut impl FnMut(u32) -> bool,
    ended: &mut impl FnMut(u32),
) -> Result<PathBuf, WorktreeError> {
    let ran = done::run_command(
        OsStr::new("git"),
        &args(&[
            OsStr::new("-C"),
            folder.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--show-toplevel"),
            OsStr::new("--git-dir"),
            OsStr::new("--git-common-dir"),
            OsStr::new("--abbrev-ref"),
            OsStr::new("HEAD"),
        ]),
        folder,
        opts,
        started,
        ended,
    )
    .map_err(|_| WorktreeError::Stopped)?;
    match ran {
        Ran::Spawn(_) | Ran::Timeout => return Err(failed(E_LANE_GIT, git_detail(&ran, opts))),
        Ran::Exited(0) => {}
        _ => return Err(failed(E_LANE_WORKTREE, output_detail(opts.out_dir))),
    }
    let lines = stdout_lines(opts.out_dir);
    let line = |index: usize| lines.get(index).map_or("", String::as_str);
    let canonical = |path: &Path| std::fs::canonicalize(path).ok();
    // A folder without a valid `.git` link resolves to an outer checkout.
    if line(0).is_empty() || canonical(Path::new(line(0))) != canonical(folder) {
        return Err(failed(E_LANE_WORKTREE, "not the lane folder"));
    }
    if line(3) != branch(lane_id) {
        return Err(failed(E_LANE_WORKTREE, "wrong branch"));
    }
    if line(2).is_empty() || canonical(&folder.join(line(2))).as_deref() != Some(common) {
        return Err(failed(E_LANE_WORKTREE, "another repo"));
    }
    let git_dir = folder.join(line(1));
    if need_marker && !git_dir.join(READY_MARKER).is_file() {
        return Err(failed(E_LANE_WORKTREE, "not finished"));
    }
    Ok(git_dir)
}

/// Reads the git state of lane worktree `folder` for a `lane_done` claim: tracked changes not
/// committed, and commits of HEAD on no remote (only when a remote exists). Untracked files never
/// count. Any read that fails gives exactly one `E_CHECK_GIT` check. `started`/`ended` see every
/// git pid as in [`done::run_command`].
pub(crate) fn git_checks(
    folder: &Path,
    opts: &RunOpts<'_>,
    mut started: impl FnMut(u32) -> bool,
    mut ended: impl FnMut(u32),
) -> Result<Vec<Check>, Stopped> {
    let env = git_env(opts.env);
    let opts = RunOpts {
        out_dir: opts.out_dir,
        env: &env,
        timeout: opts.timeout,
    };
    let git = OsStr::new("git");
    let dash_c = OsStr::new("-C");
    let unreadable = |detail: String| {
        vec![Check {
            signal: "git state".to_owned(),
            code: Some(E_CHECK_GIT),
            detail: done::short(&detail),
        }]
    };

    // 1. The folder is its own top level (a lost `.git` link resolves to an outer checkout).
    let ran = done::run_command(
        git,
        &args(&[
            dash_c,
            folder.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--show-toplevel"),
        ]),
        folder,
        &opts,
        &mut started,
        &mut ended,
    )?;
    if !matches!(ran, Ran::Exited(0)) {
        return Ok(unreadable(git_detail(&ran, &opts)));
    }
    let lines = stdout_lines(opts.out_dir);
    let top = lines.first().map(std::fs::canonicalize);
    let same = match (top, std::fs::canonicalize(folder)) {
        (Some(Ok(top)), Ok(folder)) => top == folder,
        _ => false,
    };
    if !same {
        return Ok(unreadable("not the lane folder".into()));
    }

    // 2. Changes to tracked files, staged or not; untracked files are not listed.
    let ran = done::run_command(
        git,
        &args(&[
            dash_c,
            folder.as_os_str(),
            OsStr::new("status"),
            OsStr::new("--porcelain"),
            OsStr::new("--untracked-files=no"),
        ]),
        folder,
        &opts,
        &mut started,
        &mut ended,
    )?;
    if !matches!(ran, Ran::Exited(0)) {
        return Ok(unreadable(git_detail(&ran, &opts)));
    }
    let changed = stdout_lines(opts.out_dir)
        .iter()
        .filter(|line| !line.is_empty())
        .count();
    let committed = if changed > 0 {
        Check {
            signal: "git committed".to_owned(),
            code: Some(E_CHECK_UNCOMMITTED),
            detail: format!("{changed} tracked files changed"),
        }
    } else {
        Check {
            signal: "git committed".to_owned(),
            code: None,
            detail: "clean".to_owned(),
        }
    };

    // 3. Whether any remote is configured.
    let ran = done::run_command(
        git,
        &args(&[dash_c, folder.as_os_str(), OsStr::new("remote")]),
        folder,
        &opts,
        &mut started,
        &mut ended,
    )?;
    if !matches!(ran, Ran::Exited(0)) {
        return Ok(unreadable(git_detail(&ran, &opts)));
    }
    if stdout_lines(opts.out_dir).iter().all(String::is_empty) {
        let pushed = Check {
            signal: "git pushed (no remote)".to_owned(),
            code: None,
            detail: "no remote".to_owned(),
        };
        return Ok(vec![committed, pushed]);
    }

    // 4. Commits of HEAD on no remote-tracking ref (no upstream needed).
    let ran = done::run_command(
        git,
        &args(&[
            dash_c,
            folder.as_os_str(),
            OsStr::new("rev-list"),
            OsStr::new("--count"),
            OsStr::new("HEAD"),
            OsStr::new("--not"),
            OsStr::new("--remotes"),
        ]),
        folder,
        &opts,
        &mut started,
        &mut ended,
    )?;
    if !matches!(ran, Ran::Exited(0)) {
        return Ok(unreadable(git_detail(&ran, &opts)));
    }
    let lines = stdout_lines(opts.out_dir);
    let Some(unpushed) = lines.first().and_then(|line| line.parse::<u64>().ok()) else {
        return Ok(unreadable("bad commit count".into()));
    };
    let pushed = if unpushed > 0 {
        Check {
            signal: "git pushed".to_owned(),
            code: Some(E_CHECK_UNPUSHED),
            detail: format!("{unpushed} commits on no remote"),
        }
    } else {
        Check {
            signal: "git pushed".to_owned(),
            code: None,
            detail: "pushed".to_owned(),
        }
    };
    Ok(vec![committed, pushed])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const GIT_REQUIRED: &str = "git is required for the lanes tests";

    fn env(dir: &Path, path: &OsStr) -> Vec<(OsString, OsString)> {
        vec![
            ("PATH".into(), path.to_os_string()),
            ("HOME".into(), dir.join("qa-mbhome").into_os_string()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ]
    }

    fn opts<'a>(out: &'a Path, env: &'a [(OsString, OsString)], timeout: Duration) -> RunOpts<'a> {
        RunOpts {
            out_dir: out,
            env,
            timeout,
        }
    }

    fn real_path() -> OsString {
        std::env::var_os("PATH").expect("PATH")
    }

    /// Writes `<dir>/<name>/git` (a `/bin/sh` script running `body`) through a child, so this
    /// process never holds a write handle to a file it later executes.
    fn fake_git(dir: &Path, name: &str, body: &str) -> PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        let status = Command::new("/bin/sh")
            .args([
                "-c",
                r##"printf "%s\n" "#!/bin/sh" "$2" > "$1"; chmod 755 "$1""##,
                "sh",
            ])
            .arg(folder.join("git"))
            .arg(body)
            .status()
            .expect("sh");
        assert!(status.success());
        folder
    }

    fn require_git() {
        let output = Command::new("git")
            .arg("--version")
            .output()
            .expect(GIT_REQUIRED);
        assert!(output.status.success(), "{GIT_REQUIRED}");
    }

    fn git(dir: &Path, args: &[&OsStr]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(env(dir, &real_path()))
            .output()
            .expect(GIT_REQUIRED);
        assert!(output.status.success(), "{GIT_REQUIRED}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn qa_repo(dir: &Path, name: &str) -> PathBuf {
        let repo = dir.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(
            &repo,
            &[
                "init".as_ref(),
                "-q".as_ref(),
                "-b".as_ref(),
                "main".as_ref(),
            ],
        );
        std::fs::write(repo.join("tracked.txt"), "x").unwrap();
        git(&repo, &["add".as_ref(), "tracked.txt".as_ref()]);
        git(
            &repo,
            &[
                "-c".as_ref(),
                "user.name=qa".as_ref(),
                "-c".as_ref(),
                "user.email=qa@example.invalid".as_ref(),
                "commit".as_ref(),
                "-q".as_ref(),
                "-m".as_ref(),
                "init".as_ref(),
            ],
        );
        std::fs::canonicalize(repo).unwrap()
    }

    fn worktrees(repo: &Path) -> Vec<String> {
        git(
            repo,
            &[
                "-C".as_ref(),
                repo.as_os_str(),
                "worktree".as_ref(),
                "list".as_ref(),
                "--porcelain".as_ref(),
            ],
        )
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .map(str::to_owned)
        .collect()
    }

    fn out(dir: &Path) -> PathBuf {
        let out = dir.join("qa-mbhome").join("git");
        omp::private_dir(&out).unwrap();
        out
    }

    fn lane_folder(dir: &Path, lane_id: i64) -> PathBuf {
        folder(&dir.join("work"), lane_id)
    }

    fn code(result: Result<(), WorktreeError>) -> &'static str {
        match result {
            Err(WorktreeError::Failed { code, .. }) => code,
            other => panic!("expected a coded failure, got {other:?}"),
        }
    }

    /// `ensure` with the default values: real `PATH`, `GIT_TIMEOUT`, counting closures.
    fn ensure_counted(
        dir: &Path,
        repo: &Path,
        lane_id: i64,
        path: &OsStr,
        counts: &mut (u32, u32),
    ) -> Result<(), WorktreeError> {
        let (out, env) = (out(dir), env(dir, path));
        let (mut started, mut ended) = (0, 0);
        let result = ensure(
            repo,
            &lane_folder(dir, lane_id),
            lane_id,
            &opts(&out, &env, GIT_TIMEOUT),
            |_| {
                started += 1;
                true
            },
            |_| ended += 1,
        );
        counts.0 += started;
        counts.1 += ended;
        result
    }

    #[test]
    fn ensure_creates_then_reuses_a_worktree() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        let folder = lane_folder(dir, 1);
        let mut counts = (0, 0);
        ensure_counted(dir, &repo, 1, &real_path(), &mut counts).unwrap();
        assert!(folder.join("tracked.txt").is_file());
        let head = git(
            dir,
            &[
                "-C".as_ref(),
                folder.as_os_str(),
                "rev-parse".as_ref(),
                "--abbrev-ref".as_ref(),
                "HEAD".as_ref(),
            ],
        );
        assert_eq!(head.trim(), "moraebot/lane-1");
        assert!(
            repo.join(".git/worktrees/lane-1")
                .join(READY_MARKER)
                .is_file()
        );
        assert_eq!(worktrees(&repo).len(), 2);
        ensure_counted(dir, &repo, 1, &real_path(), &mut counts).unwrap();
        assert_eq!(worktrees(&repo).len(), 2);
        assert_eq!(counts, (7, 7));
    }

    #[test]
    fn ensure_refuses_a_half_made_folder() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        let mut counts = (0, 0);
        let plain = lane_folder(dir, 1);
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            code(ensure_counted(dir, &repo, 1, &real_path(), &mut counts)),
            E_LANE_WORKTREE
        );
        let unmarked = lane_folder(dir, 2);
        git(
            dir,
            &[
                "-C".as_ref(),
                repo.as_os_str(),
                "worktree".as_ref(),
                "add".as_ref(),
                "-q".as_ref(),
                "-b".as_ref(),
                "moraebot/lane-2".as_ref(),
                unmarked.as_os_str(),
                "HEAD".as_ref(),
            ],
        );
        assert_eq!(
            code(ensure_counted(dir, &repo, 2, &real_path(), &mut counts)),
            E_LANE_WORKTREE
        );
        assert_eq!(worktrees(&repo).len(), 2);
        assert!(plain.is_dir() && unmarked.is_dir());
    }

    #[test]
    fn ensure_refuses_another_repos_worktree() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo_b = qa_repo(dir, "qa-repo-b");
        let repo = qa_repo(dir, "qa-repo");
        let mut counts = (0, 0);
        ensure_counted(dir, &repo_b, 1, &real_path(), &mut counts).unwrap();
        assert_eq!(
            code(ensure_counted(dir, &repo, 1, &real_path(), &mut counts)),
            E_LANE_WORKTREE
        );
        assert_eq!(worktrees(&repo).len(), 1);
    }

    #[test]
    fn ensure_reports_a_git_that_cannot_run() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let stub = fake_git(
            dir,
            "qa-stubgit",
            "echo 'xcrun: error: qa stub' >&2; exit 1",
        );
        let plain = dir.join("qa-plain");
        std::fs::create_dir_all(&plain).unwrap();
        let mut counts = (0, 0);
        assert_eq!(
            code(ensure_counted(
                dir,
                &plain,
                1,
                stub.as_os_str(),
                &mut counts
            )),
            E_LANE_GIT
        );
        assert_eq!(counts.0, 1);
        assert!(!lane_folder(dir, 1).exists());
    }

    #[test]
    fn ensure_refuses_a_folder_that_is_not_a_checkout() {
        require_git();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let plain = dir.join("qa-plain");
        std::fs::create_dir_all(&plain).unwrap();
        let mut counts = (0, 0);
        match ensure_counted(dir, &plain, 1, &real_path(), &mut counts) {
            Err(WorktreeError::Failed { code, detail }) => {
                assert_eq!(code, E_LANE_REPO);
                assert!(!detail.is_empty());
            }
            other => panic!("expected E_LANE_REPO, got {other:?}"),
        }
        assert!(!lane_folder(dir, 1).exists());
    }

    #[test]
    fn ensure_reports_a_failed_add() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let empty = dir.join("qa-empty");
        std::fs::create_dir_all(&empty).unwrap();
        git(
            &empty,
            &[
                "init".as_ref(),
                "-q".as_ref(),
                "-b".as_ref(),
                "main".as_ref(),
            ],
        );
        let mut counts = (0, 0);
        assert_eq!(
            code(ensure_counted(dir, &empty, 1, &real_path(), &mut counts)),
            E_LANE_WORKTREE
        );
    }

    #[test]
    fn ensure_reports_missing_git() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let plain = dir.join("qa-plain");
        let nogit = dir.join("qa-nogit");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::create_dir_all(&nogit).unwrap();
        let mut counts = (0, 0);
        assert_eq!(
            code(ensure_counted(
                dir,
                &plain,
                1,
                nogit.as_os_str(),
                &mut counts
            )),
            E_LANE_GIT
        );
    }

    #[test]
    fn git_timeout_is_e_lane_git() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let fake = fake_git(dir, "qa-fakegit", "exec /bin/sleep 600");
        let plain = dir.join("qa-plain");
        std::fs::create_dir_all(&plain).unwrap();
        let (out, env) = (out(dir), env(dir, fake.as_os_str()));
        let mut pid = 0;
        let result = ensure(
            &plain,
            &lane_folder(dir, 1),
            1,
            &opts(&out, &env, Duration::from_secs(1)),
            |started| {
                pid = started;
                true
            },
            |_| {},
        );
        eprintln!("qa-fakegit pgid {pid}");
        assert_eq!(code(result), E_LANE_GIT);
        assert_ne!(pid, 0);
        let alive = unsafe { libc::kill(-(pid as libc::pid_t), 0) };
        assert_eq!(alive, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn ensure_stops_when_started_refuses() {
        require_git();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        let (out, env) = (out(dir), env(dir, &real_path()));
        let (mut started, mut ended) = (0, 0);
        let result = ensure(
            &repo,
            &lane_folder(dir, 1),
            1,
            &opts(&out, &env, GIT_TIMEOUT),
            |_| {
                started += 1;
                false
            },
            |_| ended += 1,
        );
        assert!(matches!(result, Err(WorktreeError::Stopped)));
        assert!(!lane_folder(dir, 1).exists());
        assert_eq!((started, ended), (1, 1));
    }

    #[test]
    fn folder_and_branch_names() {
        assert_eq!(
            folder(Path::new("/w"), 7),
            PathBuf::from("/w/.moraebot-lanes/lane-7")
        );
        assert_eq!(branch(7), "moraebot/lane-7");
    }

    /// A local bare repo `<dir>/qa-remote.git` as `origin` of `repo`, with `main` pushed.
    fn qa_remote(dir: &Path, repo: &Path) -> PathBuf {
        git(
            dir,
            &[
                "init".as_ref(),
                "-q".as_ref(),
                "--bare".as_ref(),
                "qa-remote.git".as_ref(),
            ],
        );
        let remote = dir.join("qa-remote.git");
        git(
            repo,
            &[
                "remote".as_ref(),
                "add".as_ref(),
                "origin".as_ref(),
                remote.as_os_str(),
            ],
        );
        git(
            repo,
            &[
                "push".as_ref(),
                "-q".as_ref(),
                "origin".as_ref(),
                "main".as_ref(),
            ],
        );
        remote
    }

    fn commit_all(folder: &Path, message: &str) {
        git(
            folder,
            &[
                "-c".as_ref(),
                "user.name=qa".as_ref(),
                "-c".as_ref(),
                "user.email=qa@example.invalid".as_ref(),
                "commit".as_ref(),
                "-q".as_ref(),
                "-am".as_ref(),
                message.as_ref(),
            ],
        );
    }

    /// Lane 1's worktree of `repo`, made by `ensure`.
    fn lane_worktree(dir: &Path, repo: &Path) -> PathBuf {
        ensure_counted(dir, repo, 1, &real_path(), &mut (0, 0)).expect("lane worktree");
        lane_folder(dir, 1)
    }

    /// The codes of `git_checks` on `folder` with `GIT_TIMEOUT` and counting closures.
    fn check_codes(
        dir: &Path,
        folder: &Path,
        path: &OsStr,
        counts: &mut (u32, u32),
    ) -> Vec<Option<&'static str>> {
        let (out, env) = (out(dir), env(dir, path));
        let (mut started, mut ended) = (0, 0);
        let checks = git_checks(
            folder,
            &opts(&out, &env, GIT_TIMEOUT),
            |_| {
                started += 1;
                true
            },
            |_| ended += 1,
        )
        .expect("not stopped");
        counts.0 += started;
        counts.1 += ended;
        checks.iter().map(|check| check.code).collect()
    }

    #[test]
    fn git_checks_pass_on_a_clean_pushed_worktree() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        qa_remote(dir, &repo);
        let folder = lane_worktree(dir, &repo);
        let mut counts = (0, 0);
        assert_eq!(
            check_codes(dir, &folder, &real_path(), &mut counts),
            [None, None]
        );
        assert_eq!(counts, (4, 4));
    }

    #[test]
    fn git_checks_refuse_uncommitted_tracked_changes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        qa_remote(dir, &repo);
        let folder = lane_worktree(dir, &repo);
        std::fs::write(folder.join("tracked.txt"), "changed").unwrap();
        assert_eq!(
            check_codes(dir, &folder, &real_path(), &mut (0, 0)),
            [Some(E_CHECK_UNCOMMITTED), None]
        );
        git(&folder, &["add".as_ref(), "tracked.txt".as_ref()]);
        assert_eq!(
            check_codes(dir, &folder, &real_path(), &mut (0, 0)),
            [Some(E_CHECK_UNCOMMITTED), None]
        );
    }

    #[test]
    fn git_checks_ignore_untracked_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        qa_remote(dir, &repo);
        let folder = lane_worktree(dir, &repo);
        std::fs::write(folder.join("qa-new.txt"), "n").unwrap();
        assert_eq!(
            check_codes(dir, &folder, &real_path(), &mut (0, 0)),
            [None, None]
        );
    }

    #[test]
    fn git_checks_refuse_unpushed_commits() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        qa_remote(dir, &repo);
        let folder = lane_worktree(dir, &repo);
        std::fs::write(folder.join("tracked.txt"), "y").unwrap();
        commit_all(&folder, "lane");
        assert_eq!(
            check_codes(dir, &folder, &real_path(), &mut (0, 0)),
            [None, Some(E_CHECK_UNPUSHED)]
        );
        git(
            &folder,
            &[
                "push".as_ref(),
                "-q".as_ref(),
                "origin".as_ref(),
                "moraebot/lane-1".as_ref(),
            ],
        );
        assert_eq!(
            check_codes(dir, &folder, &real_path(), &mut (0, 0)),
            [None, None]
        );
    }

    #[test]
    fn git_checks_without_a_remote_check_only_commits() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        let folder = lane_worktree(dir, &repo);
        std::fs::write(folder.join("tracked.txt"), "y").unwrap();
        commit_all(&folder, "lane");
        let mut counts = (0, 0);
        assert_eq!(
            check_codes(dir, &folder, &real_path(), &mut counts),
            [None, None]
        );
        assert_eq!(counts, (3, 3));
        std::fs::write(folder.join("tracked.txt"), "z").unwrap();
        assert_eq!(
            check_codes(dir, &folder, &real_path(), &mut (0, 0)),
            [Some(E_CHECK_UNCOMMITTED), None]
        );
    }

    #[test]
    fn git_checks_report_unreadable_state() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        let folder = lane_worktree(dir, &repo);

        // 1. A git that starts and exits 1 (the macOS xcrun stub).
        let stub = fake_git(
            dir,
            "qa-stubgit",
            "echo 'xcrun: error: qa stub' >&2; exit 1",
        );
        let mut counts = (0, 0);
        assert_eq!(
            check_codes(dir, &folder, stub.as_os_str(), &mut counts),
            [Some(E_CHECK_GIT)]
        );
        assert_eq!(counts, (1, 1));

        // 2. A folder outside any checkout.
        require_git();
        let plain = dir.join("qa-plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            check_codes(dir, &plain, &real_path(), &mut (0, 0)),
            [Some(E_CHECK_GIT)]
        );

        // 3. A folder inside a checkout but not its top level.
        let sub = repo.join("qa-sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            check_codes(dir, &sub, &real_path(), &mut (0, 0)),
            [Some(E_CHECK_GIT)]
        );

        // 4. A folder that does not exist.
        assert_eq!(
            check_codes(dir, &dir.join("qa-missing"), &real_path(), &mut (0, 0)),
            [Some(E_CHECK_GIT)]
        );
    }

    #[test]
    fn git_checks_stop_when_started_refuses() {
        require_git();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let repo = qa_repo(dir, "qa-repo");
        let folder = lane_worktree(dir, &repo);
        let (out, env) = (out(dir), env(dir, &real_path()));
        let (mut started, mut ended) = (0, 0);
        let result = git_checks(
            &folder,
            &opts(&out, &env, GIT_TIMEOUT),
            |_| {
                started += 1;
                false
            },
            |_| ended += 1,
        );
        assert_eq!(result, Err(Stopped));
        assert_eq!((started, ended), (1, 1));
    }
}
