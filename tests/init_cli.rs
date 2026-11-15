use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

const SENTINEL: &str = "TOKEN_SENTINEL_9f3a";

fn moraebot(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_moraebot"));
    cmd.env("MORAEBOT_HOME", home).env("HOME", home);
    cmd
}

fn fake_executable(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("omp-stand-in");
    fs::write(&path, "#!/bin/sh\n").expect("write executable");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn run_init(home: &Path, input: &str) -> std::process::Output {
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

#[test]
fn piped_init_writes_valid_config_and_private_token() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let omp = fake_executable(tmp.path());
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());

    let output = run_init(&home, &input);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );

    let token_path = home.join("secrets/discord.token");
    let mode = fs::metadata(&token_path)
        .expect("token file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(fs::read_to_string(&token_path).expect("token"), SENTINEL);

    let config_text = fs::read_to_string(home.join("config.toml")).expect("config.toml");
    for (label, text) in [("stdout", &stdout), ("stderr", &stderr)] {
        assert!(!text.contains(SENTINEL), "token leaked to {label}");
    }
    assert!(
        !config_text.contains(SENTINEL),
        "token leaked to config.toml"
    );

    let check = moraebot(&home)
        .arg("check-config")
        .output()
        .expect("check-config");
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stdout)
    );
}

#[test]
fn check_config_fails_on_loose_token_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let omp = fake_executable(tmp.path());
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());
    assert!(run_init(&home, &input).status.success());

    let token_path = home.join("secrets/discord.token");
    fs::set_permissions(&token_path, fs::Permissions::from_mode(0o644)).expect("chmod");
    let check = moraebot(&home)
        .arg("check-config")
        .output()
        .expect("check-config");
    assert_eq!(check.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&check.stdout).contains("E_TOKEN_MODE"));
}

#[test]
fn unknown_subcommand_exits_2() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = moraebot(tmp.path()).arg("bogus").output().expect("run");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn config_carries_its_own_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let omp = fake_executable(tmp.path());
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());
    assert!(run_init(&home, &input).status.success());

    let config_text = fs::read_to_string(home.join("config.toml")).expect("config.toml");
    assert!(config_text.contains("home = "));

    let other_home = tmp.path().join("other-home");
    fs::create_dir(&other_home).expect("mkdir");
    let check = Command::new(env!("CARGO_BIN_EXE_moraebot"))
        .env_remove("MORAEBOT_HOME")
        .env("HOME", &other_home)
        .arg("check-config")
        .arg("--config")
        .arg(home.join("config.toml"))
        .output()
        .expect("check-config");
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stdout));
    assert!(!other_home.join(".moraebot").exists());
}
