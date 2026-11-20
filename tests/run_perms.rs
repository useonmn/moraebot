use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use moraebot::gateway::STORE_FILE;
use moraebot::store::E_STORE;

const SENTINEL: &str = "TOKEN_SENTINEL_9f3a";

fn moraebot(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_moraebot"));
    cmd.env("MORAEBOT_HOME", home).env("HOME", home);
    cmd
}

/// Answers the start check of `moraebot run` (`omp_overlay::check`) like omp 18.22.2.
fn fake_executable(dir: &Path) -> PathBuf {
    let path = dir.join("omp-stand-in");
    let script = "#!/bin/sh\n\
                  if [ \"$1\" = --version ]; then echo omp/18.22.2; exit 0; fi\n\
                  printf '{\"key\": \"%s\"}\\n' \"$3\"\n";
    fs::write(&path, script).expect("write executable");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn run_init(home: &Path, input: &str) -> Output {
    let mut child = moraebot(home)
        .arg("init")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn moraebot");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write answers");
    child.wait_with_output().expect("wait")
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

fn assert_owner_only(home: &Path) {
    let files = files_under(home);
    assert!(!files.is_empty(), "no files under {}", home.display());
    for file in files {
        let mode = fs::metadata(&file).expect("metadata").permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{} has mode {:o}",
            file.display(),
            mode & 0o777
        );
    }
}

fn init_home(tmp: &Path) -> PathBuf {
    let home = tmp.join("home");
    let omp = fake_executable(tmp);
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());
    let output = run_init(&home, &input);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    home
}

#[test]
fn init_creates_only_owner_only_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = init_home(tmp.path());
    assert_owner_only(&home);
}

#[test]
fn run_log_files_are_owner_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = init_home(tmp.path());
    let inbox_blocked_as_directory = home.join(STORE_FILE);
    fs::create_dir(&inbox_blocked_as_directory).expect("block inbox");

    let output = moraebot(&home)
        .arg("run")
        .arg("--config")
        .arg(home.join("config.toml"))
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(E_STORE));
    assert!(!files_under(&home.join("logs")).is_empty(), "no log file");
    assert_owner_only(&home);
}
