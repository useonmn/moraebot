//! `install-systemd`, `install-launchd`, `uninstall-systemd` and the bridge line of
//! `check-config` through the real binary. None of these runs a real `systemctl` or
//! `launchctl`: dry runs only print, and a bad unit name stops before any command.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
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

/// A valid `messenger = "bridge"` config in `tmp/qa-mbhome/config.toml` with the socket
/// `tmp/run/b.sock`; returns the home, the config path and the socket path.
fn bridge_config(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let home = tmp.join("qa-mbhome");
    fs::create_dir_all(&home).expect("mkdir home");
    let omp = tmp.join("omp-stand-in");
    fs::write(&omp, "#!/bin/sh\n").expect("write omp");
    fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).expect("chmod omp");
    let work = tmp.join("work");
    fs::create_dir_all(&work).expect("mkdir work");
    let run = tmp.join("run");
    fs::create_dir_all(&run).expect("mkdir run");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).expect("chmod run");
    let socket = run.join("b.sock");
    let config = home.join("config.toml");
    fs::write(
        &config,
        format!(
            "messenger = \"bridge\"\nomp_path = \"{}\"\nhome = \"{}\"\nwork_dir = \"{}\"\n[bridge]\nsocket = \"{}\"\nowner_id = \"U111\"\n",
            omp.display(),
            home.display(),
            work.display(),
            socket.display(),
        ),
    )
    .expect("write config");
    (home, config, socket)
}

fn moraebot(home: &Path, vars: &[(&str, &Path)], args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_moraebot"));
    cmd.env_clear()
        .env("HOME", home)
        .env("MORAEBOT_HOME", home)
        .env("PATH", "/usr/bin:/bin");
    // Where the binary finds its shared libraries on a host without system ones.
    if let Some(libs) = std::env::var_os("LD_LIBRARY_PATH") {
        cmd.env("LD_LIBRARY_PATH", libs);
    }
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
fn install_systemd_dry_run_on_a_bridge_config_prints_both_units() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (home, config, _socket) = bridge_config(tmp.path());
    let xdg = tmp.path().join("xdg");
    let cfg = config.to_str().expect("utf-8 path");

    let output = moraebot(
        &home,
        &[("XDG_CONFIG_HOME", &xdg)],
        &["install-systemd", "--dry-run", "--config", cfg],
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    let units = xdg.join("systemd").join("user");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], format!("# {}/moraebot.service", units.display()));
    assert!(
        lines.contains(&format!("# {}/moraebot-slack-bridge.service", units.display()).as_str()),
        "{stdout}"
    );
    assert!(
        lines.contains(&"Wants=moraebot-slack-bridge.service"),
        "{stdout}"
    );
    assert!(
        lines.contains(
            &format!(
                "ExecStart=\"{h}/bin/moraebot-slack-bridge\" run --config \"{h}/slack-bridge.toml\"",
                h = home.display()
            )
            .as_str()
        ),
        "{stdout}"
    );
    assert_eq!(
        lines[lines.len() - 5..],
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable moraebot-slack-bridge.service",
            "systemctl --user restart moraebot-slack-bridge.service",
            "systemctl --user enable moraebot.service",
            "systemctl --user restart moraebot.service",
        ]
    );
    assert!(!xdg.exists());
}

#[test]
fn install_launchd_dry_run_on_a_bridge_config_prints_both_agents() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (home, config, _socket) = bridge_config(tmp.path());
    let cfg = config.to_str().expect("utf-8 path");

    let output = moraebot(
        &home,
        &[],
        &["install-launchd", "--dry-run", "--config", cfg],
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert_eq!(stdout.matches("<?xml").count(), 2, "{stdout}");
    assert!(
        stdout.contains("<string>dev.moraebot.gateway.slack-bridge</string>"),
        "{stdout}"
    );
    let agents = home.join("Library").join("LaunchAgents");
    let commands: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("launchctl "))
        .collect();
    assert_eq!(commands.len(), 4, "{stdout}");
    for (line, (verb, label)) in commands.iter().zip([
        ("bootout", "dev.moraebot.gateway.slack-bridge"),
        ("bootstrap", "dev.moraebot.gateway.slack-bridge"),
        ("bootout", "dev.moraebot.gateway"),
        ("bootstrap", "dev.moraebot.gateway"),
    ]) {
        assert!(
            line.starts_with(&format!("launchctl {verb} gui/")),
            "{line}"
        );
        let tail = match verb {
            "bootout" => format!("/{label}"),
            _ => format!(" {}/{label}.plist", agents.display()),
        };
        assert!(line.ends_with(&tail), "{line}");
    }
    assert!(!home.join("Library").exists());
}

/// The `bridge` line of `check-config` stdout.
fn bridge_line(stdout: &str) -> &str {
    stdout
        .lines()
        .find(|line| line.contains(" bridge: "))
        .unwrap_or_else(|| panic!("no bridge line in:\n{stdout}"))
}

#[test]
fn check_config_warns_when_no_bridge_answers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (home, config, socket) = bridge_config(tmp.path());
    let cfg = config.to_str().expect("utf-8 path");

    let output = moraebot(&home, &[], &["check-config", "--config", cfg]);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let line = bridge_line(&stdout);
    assert!(
        line.starts_with(&format!(
            "WARN bridge: nothing answers at {}: cannot connect",
            socket.display()
        )),
        "{stdout}"
    );
}

#[test]
fn check_config_reports_a_listening_bridge() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (home, config, socket) = bridge_config(tmp.path());
    let cfg = config.to_str().expect("utf-8 path");
    let listener = UnixListener::bind(&socket).expect("bind");
    let bridge = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .write_all(
                b"{\"t\":\"hello\",\"proto\":1,\"bridge\":\"fake-bridge\",\"version\":\"9.9.9\",\"platform\":\"slack\",\"message_limit\":3900,\"hints\":\"h\"}\n",
            )
            .expect("write hello");
        // The probe closes without a hello_ok.
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).expect("read to end");
        rest
    });

    let output = moraebot(&home, &[], &["check-config", "--config", cfg]);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(
        bridge_line(&stdout),
        format!(
            "OK   bridge: fake-bridge 9.9.9 (slack) at {}",
            socket.display()
        )
    );
    assert_eq!(bridge.join().expect("bridge thread"), Vec::<u8>::new());
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
