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

/// Answers the overlay check of `check-config` (`omp_overlay::check`) like omp 18.22.2.
fn fake_executable(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("omp-stand-in");
    let script = "#!/bin/sh\n\
                  if [ \"$1\" = --version ]; then echo omp/18.22.2; exit 0; fi\n\
                  printf '{\"key\": \"%s\"}\\n' \"$3\"\n";
    fs::write(&path, script).expect("write executable");
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

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn init_writes_the_onboarding_request() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("qa-mbhome");
    let omp = fake_executable(tmp.path());
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());

    let output = run_init(&home, &input);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .ends_with(moraebot::init::next_steps(std::env::consts::OS))
    );
    assert_eq!(mode_of(&home.join("onboarding.request")), 0o600);
    assert_eq!(
        fs::read_to_string(home.join("onboarding.request")).expect("request"),
        "dm\n"
    );
}

#[test]
fn onboarding_command_writes_the_request() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = home_without_request(tmp.path());
    let request = home.join("onboarding.request");

    let output = moraebot(&home)
        .arg("onboarding")
        .output()
        .expect("onboarding");
    assert_eq!(output.status.code(), Some(0));
    let printed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    assert_eq!(
        printed,
        serde_json::json!({"onboarding": "requested", "greet": "dm"})
    );
    assert_eq!(mode_of(&request), 0o600);
    assert_eq!(fs::read_to_string(&request).expect("request"), "dm\n");
    assert!(!home.join("inbox.sqlite").exists());
}

/// A home set up by `init` whose onboarding request was removed again.
fn home_without_request(tmp: &Path) -> std::path::PathBuf {
    let home = tmp.join("qa-mbhome");
    let omp = fake_executable(tmp);
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());
    assert!(run_init(&home, &input).status.success());
    fs::remove_file(home.join("onboarding.request")).expect("init left a request");
    home
}

#[test]
fn onboarding_command_can_greet_in_the_channel() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = home_without_request(tmp.path());

    let output = moraebot(&home)
        .args(["onboarding", "--greet", "channel", "--config"])
        .arg(home.join("config.toml"))
        .output()
        .expect("onboarding");
    assert_eq!(output.status.code(), Some(0));
    let printed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    assert_eq!(
        printed,
        serde_json::json!({"onboarding": "requested", "greet": "channel"})
    );
    assert_eq!(
        fs::read_to_string(home.join("onboarding.request")).expect("request"),
        "channel\n"
    );
}

#[test]
fn onboarding_command_refuses_an_unknown_place() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = home_without_request(tmp.path());

    let output = moraebot(&home)
        .args(["onboarding", "--greet", "thread", "--config"])
        .arg(home.join("config.toml"))
        .output()
        .expect("onboarding");
    assert_ne!(output.status.code(), Some(0));
    assert!(!home.join("onboarding.request").exists());
}

#[test]
fn onboarding_command_without_config_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("qa-mbhome");
    fs::create_dir(&home).expect("mkdir");
    let output = moraebot(&home)
        .arg("onboarding")
        .output()
        .expect("onboarding");
    assert_eq!(output.status.code(), Some(1));
    assert!(!home.join("onboarding.request").exists());
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

/// moraedang on #46: `check-config` never prints a jev key written into `config.toml`.
#[test]
fn check_config_never_prints_a_key_written_into_the_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let omp = fake_executable(tmp.path());
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());
    assert!(run_init(&home, &input).status.success());
    let config = home.join("config.toml");
    let mut text = fs::read_to_string(&config).expect("config.toml");
    text.push_str("\n[memory.jev]\napi_key = \"JEV_KEY_SENTINEL_5a0c\"\n");
    fs::write(&config, text).expect("write config");

    let check = moraebot(&home)
        .arg("check-config")
        .output()
        .expect("check-config");
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    assert_eq!(check.status.code(), Some(1), "{out}");
    assert!(out.contains("E_CONFIG_PARSE"), "{out}");
    assert!(!out.contains("JEV_KEY_SENTINEL_5a0c"), "{out}");
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
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stdout)
    );
    assert!(!other_home.join(".moraebot").exists());
}

const BOT: &str = "xoxb-test-SENTINEL1";
const APP: &str = "xapp-test-SENTINEL2";

fn run_with_input(home: &Path, args: &[&str], input: &str) -> std::process::Output {
    let mut child = moraebot(home)
        .args(args)
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

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("hex digits");
            decoded.push(u8::from_str_radix(hex, 16).expect("hex"));
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).expect("utf-8")
}

#[test]
fn init_then_setup_slack_keeps_tokens_private_and_activates_slack() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let omp = fake_executable(tmp.path());
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());
    assert!(run_init(&home, &input).status.success());

    let output = run_with_input(
        &home,
        &["setup", "slack"],
        &format!("U111\nC222\n{BOT}\n{APP}\n"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");

    for name in ["slack-bot.token", "slack-app.token"] {
        let mode = fs::metadata(home.join("secrets").join(name))
            .expect("token file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    let config_text = fs::read_to_string(home.join("config.toml")).expect("config.toml");
    assert!(config_text.contains("messenger = \"slack\""));
    assert!(config_text.contains("[discord]") && config_text.contains("[slack]"));
    for text in [stdout.as_ref(), config_text.as_str()] {
        assert!(!text.contains("SENTINEL"), "token leaked");
    }

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
fn setup_prints_manifest_link_that_decodes_to_manifest() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let omp = fake_executable(tmp.path());
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());
    assert!(run_init(&home, &input).status.success());

    let output = run_with_input(
        &home,
        &["setup", "slack"],
        &format!("U111\nC222\n{BOT}\n{APP}\n"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let link = stdout
        .lines()
        .find(|line| line.starts_with("https://api.slack.com/apps?new_app=1&manifest_json="))
        .expect("link line");
    let encoded = link.split("manifest_json=").nth(1).expect("parameter");
    let printed: serde_json::Value =
        serde_json::from_str(&percent_decode(encoded)).expect("decoded manifest is JSON");
    let file: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/guides/slack-manifest.json"
        ))
        .expect("manifest file"),
    )
    .expect("manifest JSON");
    assert_eq!(printed, file);
}

#[test]
fn failed_setup_leaves_config_byte_identical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let omp = fake_executable(tmp.path());
    let input = format!("1\n111\n222\n{SENTINEL}\n{}\n\n", omp.display());
    assert!(run_init(&home, &input).status.success());
    let before = fs::read(home.join("config.toml")).expect("config");

    let output = run_with_input(&home, &["setup", "slack"], "U111\nC222\nxapp-test-x\n");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("E_SLACK_TOKEN_KIND"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_INIT_EOF"));
    assert_eq!(fs::read(home.join("config.toml")).expect("config"), before);
}

#[test]
fn setup_without_config_fails_with_its_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_with_input(&tmp.path().join("home"), &["setup", "discord"], "");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_SETUP_NO_CONFIG"));
}

#[test]
fn guide_prints_embedded_guides() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["discord", "slack"] {
        let output = moraebot(tmp.path())
            .args(["guide", name])
            .output()
            .expect("guide");
        assert!(output.status.success());
        let file = fs::read_to_string(format!(
            "{}/docs/guides/{name}.md",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("guide file");
        assert_eq!(String::from_utf8_lossy(&output.stdout), file);
    }
}

#[test]
fn unknown_messenger_is_usage_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for args in [["setup", "telegram"], ["guide", "telegram"]] {
        let output = moraebot(tmp.path()).args(args).output().expect("run");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}
