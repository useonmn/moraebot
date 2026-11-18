//! `install-systemd` and `uninstall-systemd` through the real binary. None of these runs a real
//! `systemctl`: dry runs only print, and a bad unit name stops before any command.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A valid config in `tmp/qa-mbhome/config.toml`; returns the home and the config path.
fn qa_config(tmp: &Path) -> (PathBuf, PathBuf) {
    let home = tmp.join("qa-mbhome");
    fs::create_dir_all(&home).expect("mkdir home");
    let token = tmp.join("token");
    fs::write(&token, "placeholder").expect("write token");
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("chmod token");
    let omp = tmp.join("omp-stand-in");
    fs::write(&omp, "#!/bin/sh\n").expect("write omp");
    fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).expect("chmod omp");
    let work = tmp.join("work");
    fs::create_dir_all(&work).expect("mkdir work");
    let config = home.join("config.toml");
    fs::write(
        &config,
        format!(
            "owner_id = 111\ntoken_file = \"{}\"\nomp_path = \"{}\"\nhome = \"{}\"\nwork_dir = \"{}\"\n[discord]\nchannel_id = 222\n",
            token.display(),
            omp.display(),
            home.display(),
            work.display(),
        ),
    )
    .expect("write config");
    (home, config)
}

fn moraebot(home: &Path, vars: &[(&str, &Path)], args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_moraebot"));
    cmd.env_clear()
        .env("HOME", home)
        .env("MORAEBOT_HOME", home)
        .env("PATH", "/usr/bin:/bin");
    for (name, value) in vars {
        cmd.env(name, value);
    }
    cmd.args(args).output().expect("run moraebot")
}

fn systemctl_lines(unit: &str) -> Vec<String> {
    vec![
        "systemctl --user daemon-reload".to_string(),
        format!("systemctl --user enable {unit}.service"),
        format!("systemctl --user restart {unit}.service"),
    ]
}

#[test]
fn install_systemd_dry_run_uses_xdg_config_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (home, config) = qa_config(tmp.path());
    let xdg = tmp.path().join("xdg");
    let cfg = config.to_str().expect("utf-8 path");

    let output = moraebot(
        &home,
        &[("XDG_CONFIG_HOME", &xdg)],
        &[
            "install-systemd",
            "--dry-run",
            "--config",
            cfg,
            "--unit",
            "moraebot-qa",
        ],
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "# {}/xdg/systemd/user/moraebot-qa.service",
            tmp.path().display()
        )
    );
    assert_eq!(lines[lines.len() - 3..], systemctl_lines("moraebot-qa"));
    assert!(!xdg.exists());
}

#[test]
fn install_systemd_dry_run_defaults_to_dot_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (home, config) = qa_config(tmp.path());
    let cfg = config.to_str().expect("utf-8 path");

    let output = moraebot(
        &home,
        &[],
        &["install-systemd", "--dry-run", "--config", cfg],
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "# {}/qa-mbhome/.config/systemd/user/moraebot.service",
            tmp.path().display()
        )
    );
    assert_eq!(lines[lines.len() - 3..], systemctl_lines("moraebot"));
    assert!(!home.join(".config").exists());
}

#[test]
fn uninstall_systemd_rejects_a_bad_unit_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (home, _config) = qa_config(tmp.path());
    let xdg = tmp.path().join("xdg");

    let output = moraebot(
        &home,
        &[("XDG_CONFIG_HOME", &xdg)],
        &["uninstall-systemd", "--unit", "bad unit"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    // macOS stops at the platform guard, Linux at the name check; both come before any command.
    assert!(
        stderr.contains("E_SERVICE_PLATFORM") || stderr.contains("E_LABEL_INVALID"),
        "{stderr}"
    );
    assert!(!stderr.contains("E_SYSTEMCTL"), "{stderr}");
    assert!(!stderr.contains("E_IO"), "{stderr}");
    assert!(!xdg.exists());
    assert!(!home.join(".config").exists());
}
