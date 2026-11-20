//! The Linux background service: a systemd --user unit that keeps `moraebot run` going,
//! the counterpart of `launchd` on macOS. With `messenger = "bridge"` a second unit keeps
//! `moraebot-slack-bridge run` going next to it.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, ConfigError, E_HOME_UNKNOWN, E_IO, E_OMP_MISSING, Env, Platform};
use crate::launchd::{self, BRIDGE_BINARY, BRIDGE_LOGS, CORE_LOGS};
use crate::slack_bridge::config::FILE_NAME as BRIDGE_CONFIG_FILE;

/// Unit name (without `.service`) used when `--unit` is not given.
pub const DEFAULT_UNIT: &str = "moraebot";
pub const E_UNIT_PATH: &str = "E_UNIT_PATH"; // a path a unit file cannot hold
pub const E_SYSTEMCTL: &str = "E_SYSTEMCTL"; // systemctl could not run or failed

/// The program `install` and `uninstall` run; tests pass `/bin/sh <fake script>` instead.
const SYSTEMCTL: &str = "systemctl";

/// Printed after a real install: how to keep the service running after logout.
pub const LINGER_HINT: &str =
    "To keep moraebot running after you log out, run: loginctl enable-linger";

/// `<config>/systemd/user`, `<config>` = `xdg_config_home` when it is absolute, else `$HOME/.config`.
/// `E_HOME_UNKNOWN` when neither is usable.
pub fn unit_dir(env: &Env, xdg_config_home: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if let Some(config) = xdg_config_home.filter(|path| path.is_absolute()) {
        return Ok(config.join("systemd").join("user"));
    }
    let home = env.home_dir.as_ref().ok_or_else(|| {
        ConfigError::new(
            E_HOME_UNKNOWN,
            "HOME is not set, cannot find ~/.config/systemd/user",
        )
    })?;
    Ok(home.join(".config").join("systemd").join("user"))
}

/// The unit name (without `.service`) of the Slack bridge that goes with the unit `unit`.
pub fn bridge_unit(unit: &str) -> String {
    format!("{unit}-slack-bridge")
}

/// The unit text for `<home>/bin/moraebot run --config <home>/config.toml`. With
/// `bridge` (a unit name without `.service`) the unit wants and starts after that unit;
/// `Wants`, not `Requires`, so stopping the bridge never stops the gateway.
pub fn render_unit(
    home: &Path,
    omp_path: &Path,
    bridge: Option<&str>,
) -> Result<String, ConfigError> {
    let omp_dir = omp_path.parent().ok_or_else(|| {
        ConfigError::new(
            E_OMP_MISSING,
            format!("omp_path {} has no folder", omp_path.display()),
        )
    })?;
    let home = unit_path(home)?;
    let omp_dir = unit_path(omp_dir)?;
    let wants = bridge
        .map(|unit| format!("Wants={unit}.service\nAfter={unit}.service\n"))
        .unwrap_or_default();
    Ok(format!(
        r#"[Unit]
Description=moraebot gateway
{wants}
[Service]
ExecStart="{home}/bin/moraebot" run --config "{home}/config.toml"
WorkingDirectory={home}
Environment="PATH={omp_dir}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
Environment="MORAEBOT_HOME={home}"
Restart=always
RestartSec=10
TimeoutStopSec={timeout}
UMask=0077
StandardOutput=append:{home}/logs/moraebot.out.log
StandardError=append:{home}/logs/moraebot.err.log

[Install]
WantedBy=default.target
"#,
        timeout = launchd::EXIT_TIMEOUT_SECS,
    ))
}

/// The unit text for `<home>/bin/moraebot-slack-bridge run --config <home>/slack-bridge.toml`.
pub fn render_bridge_unit(home: &Path) -> Result<String, ConfigError> {
    let home_text = unit_path(home)?;
    let config_file = home.join(BRIDGE_CONFIG_FILE);
    let config = unit_path(&config_file)?;
    let [out_log, err_log] = BRIDGE_LOGS;
    Ok(format!(
        r#"[Unit]
Description=moraebot Slack bridge

[Service]
ExecStart="{home_text}/bin/{BRIDGE_BINARY}" run --config "{config}"
WorkingDirectory={home_text}
Environment="MORAEBOT_HOME={home_text}"
Restart=always
RestartSec=10
TimeoutStopSec={timeout}
UMask=0077
StandardOutput=append:{home_text}/logs/{out_log}
StandardError=append:{home_text}/logs/{err_log}

[Install]
WantedBy=default.target
"#,
        timeout = launchd::EXIT_TIMEOUT_SECS,
    ))
}

/// Installs (or upgrades and restarts) the user service named `unit` (`<unit>.service` in
/// `unit_dir`). With `messenger = "bridge"` it installs `<unit>-slack-bridge.service` too;
/// otherwise it removes a bridge unit left from an earlier install, so the installed units
/// always match the config.
pub fn install(
    config: &Config,
    unit_dir: &Path,
    unit: &str,
    dry_run: bool,
    out: &mut impl Write,
) -> Result<(), ConfigError> {
    let source = std::env::current_exe()
        .map_err(|e| ConfigError::io(E_IO, "cannot find the running moraebot binary", &e))?;
    install_with(
        std::env::consts::OS,
        &[OsStr::new(SYSTEMCTL)],
        &source,
        config,
        unit_dir,
        unit,
        dry_run,
        out,
    )
}

/// Stops and removes the user service named `unit` and its Slack bridge unit; fine when
/// they are not installed.
pub fn uninstall(unit_dir: &Path, unit: &str, out: &mut impl Write) -> Result<(), ConfigError> {
    uninstall_with(
        std::env::consts::OS,
        &[OsStr::new(SYSTEMCTL)],
        unit_dir,
        unit,
        out,
    )
}

/// One unit file to write, enable and restart.
struct Unit {
    service: String,
    text: String,
    file: PathBuf,
}

/// `install` with the system name, the systemctl program and the `moraebot` binary to
/// copy (`source`; the bridge binary is taken from its folder) as parameters.
#[allow(clippy::too_many_arguments)]
fn install_with(
    os: &str,
    systemctl: &[&OsStr],
    source: &Path,
    config: &Config,
    unit_dir: &Path,
    unit: &str,
    dry_run: bool,
    out: &mut impl Write,
) -> Result<(), ConfigError> {
    if !dry_run {
        launchd::require_os(os, "linux")?;
    }
    launchd::check_label(unit)?;
    config.validate()?;
    let bridge_name = bridge_unit(unit);
    let with_bridge = config.messenger == Platform::Bridge;
    let make = |name: &str, text: String| Unit {
        service: format!("{name}.service"),
        file: unit_dir.join(format!("{name}.service")),
        text,
    };
    let core = make(
        unit,
        render_unit(
            &config.home,
            &config.omp_path,
            with_bridge.then_some(bridge_name.as_str()),
        )?,
    );
    let bridge = match config.messenger {
        Platform::Bridge => Some(make(&bridge_name, render_bridge_unit(&config.home)?)),
        Platform::Discord => None,
    };
    // The bridge starts first, so the gateway finds it at its first connect.
    let order: Vec<&Unit> = bridge.iter().chain([&core]).collect();
    let leftover_service = format!("{bridge_name}.service");
    let leftover = unit_dir.join(&leftover_service);
    let remove_leftover = bridge.is_none() && fs::symlink_metadata(&leftover).is_ok();
    // Checked before the dry run too, so `--dry-run` fails where the real run would.
    let bridge_binary = match bridge {
        Some(_) => Some(launchd::bridge_source(source)?),
        None => None,
    };
    if dry_run {
        for unit in [&core].into_iter().chain(&bridge) {
            writeln!(out, "# {}", unit_path(&unit.file)?)?;
            write!(out, "{}", unit.text)?;
        }
        if remove_leftover {
            writeln!(out, "systemctl --user disable --now {leftover_service}")?;
            writeln!(out, "rm {}", leftover.display())?;
        }
        writeln!(out, "systemctl --user daemon-reload")?;
        for unit in &order {
            writeln!(out, "systemctl --user enable {}", unit.service)?;
            writeln!(out, "systemctl --user restart {}", unit.service)?;
        }
        return Ok(());
    }

    let bin_dir = config.home.join("bin");
    fs::create_dir_all(&bin_dir)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot create {}", bin_dir.display()), &e))?;
    launchd::install_binary(source, &bin_dir.join("moraebot"))?;
    if let Some(binary) = &bridge_binary {
        launchd::install_binary(binary, &bin_dir.join(BRIDGE_BINARY))?;
    }
    fs::create_dir_all(unit_dir)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot create {}", unit_dir.display()), &e))?;
    launchd::prepare_log_files(&config.home, &CORE_LOGS)?;
    if bridge.is_some() {
        launchd::prepare_log_files(&config.home, &BRIDGE_LOGS)?;
    }
    for unit in &order {
        fs::write(&unit.file, &unit.text).map_err(|e| {
            ConfigError::io(E_IO, &format!("cannot write {}", unit.file.display()), &e)
        })?;
    }

    if remove_leftover {
        systemctl_user(systemctl, &["disable", "--now", &leftover_service])?;
        launchd::remove_plist(&leftover)?;
        writeln!(out, "removed {leftover_service}")?;
    }
    systemctl_user(systemctl, &["daemon-reload"])?;
    for unit in &order {
        systemctl_user(systemctl, &["enable", &unit.service])?;
        systemctl_user(systemctl, &["restart", &unit.service])?;
    }
    for unit in &order {
        writeln!(
            out,
            "installed {} at {}",
            unit.service,
            unit_path(&unit.file)?
        )?;
    }
    writeln!(out, "{LINGER_HINT}")?;
    Ok(())
}

fn uninstall_with(
    os: &str,
    systemctl: &[&OsStr],
    unit_dir: &Path,
    unit: &str,
    out: &mut impl Write,
) -> Result<(), ConfigError> {
    launchd::require_os(os, "linux")?;
    launchd::check_label(unit)?;
    let service = format!("{unit}.service");
    let bridge_service = format!("{}.service", bridge_unit(unit));
    let mut removed_bridge = false;
    // The gateway stops first, so it never sees its bridge vanish.
    for name in [&service, &bridge_service] {
        let unit_file = unit_dir.join(name);
        let present = fs::symlink_metadata(&unit_file).is_ok();
        if present {
            systemctl_user(systemctl, &["disable", "--now", name])?;
        }
        launchd::remove_plist(&unit_file)?;
        removed_bridge |= present && name == &bridge_service;
    }
    systemctl_user(systemctl, &["daemon-reload"])?;
    writeln!(out, "removed {service}")?;
    if removed_bridge {
        writeln!(out, "removed {bridge_service}")?;
    }
    Ok(())
}

/// Runs `systemctl[0] systemctl[1..] --user <args>`; non-zero exit or spawn failure is `E_SYSTEMCTL`
/// with the program's trimmed stderr.
fn systemctl_user(systemctl: &[&OsStr], args: &[&str]) -> Result<(), ConfigError> {
    let (program, prefix) = systemctl
        .split_first()
        .ok_or_else(|| ConfigError::new(E_SYSTEMCTL, "no systemctl program given"))?;
    let shown = format!("{} --user {}", program.to_string_lossy(), args.join(" "));
    let output = Command::new(program)
        .args(prefix)
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| ConfigError::io(E_SYSTEMCTL, &format!("cannot run {shown}"), &e))?;
    if !output.status.success() {
        return Err(ConfigError::new(
            E_SYSTEMCTL,
            format!(
                "{shown} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// The UTF-8 text of `path` when a unit file can hold it; `E_PATH_NOT_UTF8` or `E_UNIT_PATH`.
fn unit_path(path: &Path) -> Result<&str, ConfigError> {
    let text = launchd::utf8(path)?;
    if text
        .chars()
        .any(|c| matches!(c, '"' | '\\' | '$' | '%') || c.is_control())
    {
        return Err(ConfigError::new(
            E_UNIT_PATH,
            format!(
                "{} has a character a systemd unit cannot hold",
                path.display()
            ),
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launchd::{E_LABEL_INVALID, E_SERVICE_PLATFORM};
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const GOLDEN_UNIT: &str = r#"[Unit]
Description=moraebot gateway

[Service]
ExecStart="/tmp/moraebot test home/bin/moraebot" run --config "/tmp/moraebot test home/config.toml"
WorkingDirectory=/tmp/moraebot test home
Environment="PATH=/tmp/moraebot test omp/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
Environment="MORAEBOT_HOME=/tmp/moraebot test home"
Restart=always
RestartSec=10
TimeoutStopSec=30
UMask=0077
StandardOutput=append:/tmp/moraebot test home/logs/moraebot.out.log
StandardError=append:/tmp/moraebot test home/logs/moraebot.err.log

[Install]
WantedBy=default.target
"#;

    const GOLDEN_BRIDGE_UNIT: &str = r#"[Unit]
Description=moraebot Slack bridge

[Service]
ExecStart="/tmp/moraebot test home/bin/moraebot-slack-bridge" run --config "/tmp/moraebot test home/slack-bridge.toml"
WorkingDirectory=/tmp/moraebot test home
Environment="MORAEBOT_HOME=/tmp/moraebot test home"
Restart=always
RestartSec=10
TimeoutStopSec=30
UMask=0077
StandardOutput=append:/tmp/moraebot test home/logs/slack-bridge.out.log
StandardError=append:/tmp/moraebot test home/logs/slack-bridge.err.log

[Install]
WantedBy=default.target
"#;

    const UNIT: &str = "moraebot-qa";

    /// A valid `messenger = "bridge"` config whose home is `<dir>/qa-mbhome`.
    fn bridge_fixture(dir: &TempDir) -> Config {
        let omp = dir.path().join("omp");
        fs::write(&omp, "#!/bin/sh\n").expect("write omp");
        fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).expect("chmod omp");
        let env = Env {
            home_dir: Some(dir.path().to_path_buf()),
            moraebot_home: None,
            path: None,
        };
        let text = format!(
            "messenger = \"bridge\"\nomp_path = \"{}\"\nhome = \"{}\"\nwork_dir = \"{}\"\n[bridge]\nsocket = \"{}\"\nowner_id = \"U111\"\n",
            omp.display(),
            dir.path().join("qa-mbhome").display(),
            dir.path().join("work").display(),
            dir.path().join("run").join("b.sock").display(),
        );
        Config::from_toml(&text, &env).expect("loads")
    }

    /// A release folder holding a `moraebot` and, with `bridge`, a bridge binary; returns
    /// the `moraebot` path.
    fn release(dir: &Path, bridge: bool) -> PathBuf {
        let folder = dir.join("release");
        fs::create_dir_all(&folder).expect("mkdir release");
        let core = folder.join("moraebot");
        fs::write(&core, "core binary").expect("write core");
        if bridge {
            fs::write(folder.join(BRIDGE_BINARY), "bridge binary").expect("write bridge");
        }
        core
    }

    fn fixture(dir: &TempDir) -> Config {
        let token = dir.path().join("token");
        fs::write(&token, "placeholder").expect("write token");
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("chmod token");
        let omp = dir.path().join("omp");
        fs::write(&omp, "#!/bin/sh\n").expect("write omp");
        fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).expect("chmod omp");
        let env = Env {
            home_dir: Some(dir.path().to_path_buf()),
            moraebot_home: None,
            path: None,
        };
        let text = format!(
            "owner_id = 111\ntoken_file = \"{}\"\nomp_path = \"{}\"\nhome = \"{}\"\nwork_dir = \"{}\"\n[discord]\nchannel_id = 222\n",
            token.display(),
            omp.display(),
            dir.path().join("qa-mbhome").display(),
            dir.path().join("work").display(),
        );
        Config::from_toml(&text, &env).expect("loads")
    }

    /// A stand-in for systemctl that logs its arguments and exits with `exit`;
    /// run through `/bin/sh`, never executed directly.
    fn fake_systemctl(dir: &Path, exit: i32) -> (Vec<OsString>, PathBuf) {
        let log = dir.join("systemctl.log");
        let script = dir.join("fake-systemctl.sh");
        let mut body = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display());
        if exit != 0 {
            body.push_str("echo 'fake systemctl failure' >&2\n");
        }
        body.push_str(&format!("exit {exit}\n"));
        fs::write(&script, body).expect("write fake systemctl");
        (
            vec![OsString::from("/bin/sh"), script.into_os_string()],
            log,
        )
    }

    fn program(fake: &[OsString]) -> Vec<&OsStr> {
        fake.iter().map(OsString::as_os_str).collect()
    }

    fn logged(log: &Path) -> Vec<String> {
        match fs::read_to_string(log) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn exe() -> PathBuf {
        std::env::current_exe().expect("current exe")
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    #[test]
    fn renders_golden_unit() {
        let unit = render_unit(
            Path::new("/tmp/moraebot test home"),
            Path::new("/tmp/moraebot test omp/bin/omp"),
            None,
        )
        .expect("renders");
        assert_eq!(unit, GOLDEN_UNIT);
    }

    #[test]
    fn renders_golden_bridge_unit_and_the_gateway_wants_it() {
        let home = Path::new("/tmp/moraebot test home");
        assert_eq!(
            render_bridge_unit(home).expect("renders"),
            GOLDEN_BRIDGE_UNIT
        );
        let core = render_unit(
            home,
            Path::new("/tmp/moraebot test omp/bin/omp"),
            Some("moraebot-slack-bridge"),
        )
        .expect("renders");
        let expected = GOLDEN_UNIT.replace(
            "Description=moraebot gateway\n",
            "Description=moraebot gateway\nWants=moraebot-slack-bridge.service\nAfter=moraebot-slack-bridge.service\n",
        );
        assert_eq!(core, expected);
        assert_eq!(bridge_unit(DEFAULT_UNIT), "moraebot-slack-bridge");
        assert_eq!(
            render_bridge_unit(Path::new("/tmp/qa$mbhome\""))
                .expect_err("unsafe home")
                .code,
            E_UNIT_PATH
        );
    }

    #[test]
    fn bridge_dry_run_prints_both_units_and_starts_the_bridge_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = bridge_fixture(&dir);
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        let source = release(dir.path(), true);
        let mut out = Vec::new();
        install_with(
            "linux",
            &program(&fake),
            &source,
            &config,
            &units,
            UNIT,
            true,
            &mut out,
        )
        .expect("dry run");
        let expected = format!(
            "# {dir}/moraebot-qa.service\n{core}# {dir}/moraebot-qa-slack-bridge.service\n{bridge}\
             systemctl --user daemon-reload\n\
             systemctl --user enable moraebot-qa-slack-bridge.service\n\
             systemctl --user restart moraebot-qa-slack-bridge.service\n\
             systemctl --user enable moraebot-qa.service\n\
             systemctl --user restart moraebot-qa.service\n",
            dir = units.display(),
            core = render_unit(
                &config.home,
                &config.omp_path,
                Some("moraebot-qa-slack-bridge")
            )
            .expect("core"),
            bridge = render_bridge_unit(&config.home).expect("bridge"),
        );
        assert_eq!(String::from_utf8(out).expect("utf-8 output"), expected);
        assert_eq!(logged(&log), Vec::<String>::new());
        assert!(!units.exists());
    }

    #[test]
    fn bridge_install_writes_both_units_and_starts_the_bridge_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = bridge_fixture(&dir);
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        let source = release(dir.path(), true);
        let mut out = Vec::new();
        install_with(
            "linux",
            &program(&fake),
            &source,
            &config,
            &units,
            UNIT,
            false,
            &mut out,
        )
        .expect("installs");
        assert_eq!(
            logged(&log),
            [
                "--user daemon-reload",
                "--user enable moraebot-qa-slack-bridge.service",
                "--user restart moraebot-qa-slack-bridge.service",
                "--user enable moraebot-qa.service",
                "--user restart moraebot-qa.service",
            ]
        );
        assert_eq!(
            fs::read_to_string(units.join("moraebot-qa-slack-bridge.service"))
                .expect("bridge unit"),
            render_bridge_unit(&config.home).expect("bridge")
        );
        let core = fs::read_to_string(units.join("moraebot-qa.service")).expect("core unit");
        assert!(
            core.contains("Wants=moraebot-qa-slack-bridge.service\n"),
            "{core}"
        );
        let bin = config.home.join("bin");
        assert_eq!(
            fs::read(bin.join("moraebot")).expect("core"),
            b"core binary"
        );
        assert_eq!(
            fs::read(bin.join(BRIDGE_BINARY)).expect("bridge"),
            b"bridge binary"
        );
        assert_eq!(mode_of(&bin.join(BRIDGE_BINARY)), 0o755);
        let logs = config.home.join("logs");
        for name in CORE_LOGS.iter().chain(&BRIDGE_LOGS) {
            assert_eq!(mode_of(&logs.join(name)), 0o600, "{name}");
        }
        assert_eq!(
            String::from_utf8(out).expect("utf-8 output"),
            format!(
                "installed moraebot-qa-slack-bridge.service at {dir}/moraebot-qa-slack-bridge.service\n\
                 installed moraebot-qa.service at {dir}/moraebot-qa.service\n{LINGER_HINT}\n",
                dir = units.display()
            )
        );
    }

    #[test]
    fn a_missing_bridge_binary_writes_no_unit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = bridge_fixture(&dir);
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        let source = release(dir.path(), false);
        for dry_run in [true, false] {
            let mut out = Vec::new();
            let err = install_with(
                "linux",
                &program(&fake),
                &source,
                &config,
                &units,
                UNIT,
                dry_run,
                &mut out,
            )
            .expect_err("no bridge binary");
            assert_eq!(err.code, launchd::E_BRIDGE_BINARY, "dry run: {dry_run}");
            assert!(out.is_empty(), "dry run: {dry_run}");
        }
        assert!(!units.exists());
        assert!(!config.home.join("bin").exists());
        assert_eq!(logged(&log), Vec::<String>::new());
    }

    #[test]
    fn a_discord_install_removes_a_leftover_bridge_unit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fixture(&dir);
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        fs::create_dir_all(&units).expect("mkdir units");
        let leftover = units.join("moraebot-qa-slack-bridge.service");
        fs::write(&leftover, "[Unit]\n").expect("write leftover");

        let mut dry = Vec::new();
        install_with(
            "linux",
            &program(&fake),
            &exe(),
            &config,
            &units,
            UNIT,
            true,
            &mut dry,
        )
        .expect("dry run");
        let dry = String::from_utf8(dry).expect("utf-8 output");
        assert!(
            dry.contains(&format!(
                "systemctl --user disable --now moraebot-qa-slack-bridge.service\nrm {}\nsystemctl --user daemon-reload\n",
                leftover.display()
            )),
            "{dry}"
        );
        assert!(leftover.exists(), "dry run must not remove");

        let mut out = Vec::new();
        install_with(
            "linux",
            &program(&fake),
            &exe(),
            &config,
            &units,
            UNIT,
            false,
            &mut out,
        )
        .expect("installs");
        assert!(!leftover.exists());
        assert_eq!(
            logged(&log),
            [
                "--user disable --now moraebot-qa-slack-bridge.service",
                "--user daemon-reload",
                "--user enable moraebot-qa.service",
                "--user restart moraebot-qa.service",
            ]
        );
        let out = String::from_utf8(out).expect("utf-8 output");
        assert!(
            out.starts_with("removed moraebot-qa-slack-bridge.service\n"),
            "{out}"
        );
        assert!(
            !fs::read_to_string(units.join("moraebot-qa.service"))
                .expect("core unit")
                .contains("Wants=")
        );
    }

    #[test]
    fn uninstall_removes_the_bridge_unit_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        fs::create_dir_all(&units).expect("mkdir units");
        for name in ["moraebot-qa.service", "moraebot-qa-slack-bridge.service"] {
            fs::write(units.join(name), "[Unit]\n").expect("write unit");
        }
        let mut out = Vec::new();
        uninstall_with("linux", &program(&fake), &units, UNIT, &mut out).expect("uninstalls");
        assert_eq!(fs::read_dir(&units).expect("read units").count(), 0);
        assert_eq!(
            logged(&log),
            [
                "--user disable --now moraebot-qa.service",
                "--user disable --now moraebot-qa-slack-bridge.service",
                "--user daemon-reload",
            ]
        );
        assert_eq!(
            String::from_utf8(out).expect("utf-8 output"),
            "removed moraebot-qa.service\nremoved moraebot-qa-slack-bridge.service\n"
        );
    }

    #[test]
    fn unit_refuses_unsafe_path_characters() {
        for c in ['"', '\\', '$', '%', '\n'] {
            let home = render_unit(
                &PathBuf::from(format!("/tmp/qa{c}mbhome")),
                Path::new("/tmp/omp/omp"),
                None,
            )
            .expect_err("unsafe home");
            assert_eq!(home.code, E_UNIT_PATH, "home {c:?}");
            let omp = render_unit(
                Path::new("/tmp/qa-mbhome"),
                &PathBuf::from(format!("/tmp/omp{c}dir/omp")),
                None,
            )
            .expect_err("unsafe omp dir");
            assert_eq!(omp.code, E_UNIT_PATH, "omp {c:?}");
        }
        render_unit(
            Path::new("/tmp/qa mbhome"),
            Path::new("/tmp/omp dir/omp"),
            None,
        )
        .expect("a space alone passes");
    }

    #[test]
    fn unit_dir_follows_xdg_config_home() {
        let with_home = Env {
            home_dir: Some(PathBuf::from("/tmp/qa-mbhome")),
            moraebot_home: None,
            path: None,
        };
        let dot_config = PathBuf::from("/tmp/qa-mbhome/.config/systemd/user");
        assert_eq!(
            unit_dir(&with_home, Some(Path::new("/tmp/xdg"))).expect("absolute xdg"),
            PathBuf::from("/tmp/xdg/systemd/user")
        );
        assert_eq!(
            unit_dir(&with_home, Some(Path::new("relative/xdg"))).expect("relative xdg"),
            dot_config
        );
        assert_eq!(unit_dir(&with_home, None).expect("no xdg"), dot_config);
        let no_home = Env {
            home_dir: None,
            moraebot_home: None,
            path: None,
        };
        assert_eq!(
            unit_dir(&no_home, Some(Path::new("/tmp/xdg"))).expect("absolute xdg"),
            PathBuf::from("/tmp/xdg/systemd/user")
        );
        assert_eq!(
            unit_dir(&no_home, None).expect_err("nothing usable").code,
            E_HOME_UNKNOWN
        );
    }

    #[test]
    fn dry_run_prints_unit_and_commands_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fixture(&dir);
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        let mut out = Vec::new();
        install_with(
            "macos",
            &program(&fake),
            &exe(),
            &config,
            &units,
            UNIT,
            true,
            &mut out,
        )
        .expect("dry run passes the guard on any system");
        let expected = format!(
            "# {}/moraebot-qa.service\n{}systemctl --user daemon-reload\nsystemctl --user enable moraebot-qa.service\nsystemctl --user restart moraebot-qa.service\n",
            units.display(),
            render_unit(&config.home, &config.omp_path, None).expect("renders")
        );
        assert_eq!(String::from_utf8(out).expect("utf-8 output"), expected);
        assert_eq!(logged(&log), Vec::<String>::new());
        assert!(!units.exists());
        assert!(!config.home.join("bin").exists());
        assert!(!config.home.join("logs").join("moraebot.out.log").exists());
    }

    #[test]
    fn install_refuses_off_linux() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fixture(&dir);
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        let mut out = Vec::new();
        let err = install_with(
            "macos",
            &program(&fake),
            &exe(),
            &config,
            &units,
            "bad unit",
            false,
            &mut out,
        )
        .expect_err("refuses off Linux");
        assert_eq!(err.code, E_SERVICE_PLATFORM);
        assert!(out.is_empty());
        assert_eq!(logged(&log), Vec::<String>::new());
        assert!(!units.exists());
        let reach = install_with(
            "macos",
            &program(&fake),
            &exe(),
            &config,
            &units,
            "bad unit",
            true,
            &mut Vec::new(),
        )
        .expect_err("dry run reaches the name check");
        assert_eq!(reach.code, E_LABEL_INVALID);
    }

    #[test]
    fn install_writes_unit_and_runs_systemctl_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fixture(&dir);
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        let mut out = Vec::new();
        install_with(
            "linux",
            &program(&fake),
            &exe(),
            &config,
            &units,
            UNIT,
            false,
            &mut out,
        )
        .expect("installs");
        let unit_file = units.join("moraebot-qa.service");
        assert_eq!(
            fs::read_to_string(&unit_file).expect("read unit"),
            render_unit(&config.home, &config.omp_path, None).expect("renders")
        );
        assert_eq!(
            logged(&log),
            [
                "--user daemon-reload",
                "--user enable moraebot-qa.service",
                "--user restart moraebot-qa.service",
            ]
        );
        let binary = config.home.join("bin").join("moraebot");
        assert_eq!(mode_of(&binary), 0o755);
        let exe = std::env::current_exe().expect("current exe");
        assert_eq!(
            fs::metadata(&binary).expect("stat binary").len(),
            fs::metadata(&exe).expect("stat exe").len()
        );
        let logs = config.home.join("logs");
        assert_eq!(mode_of(&logs.join("moraebot.out.log")), 0o600);
        assert_eq!(mode_of(&logs.join("moraebot.err.log")), 0o600);
        assert_eq!(
            String::from_utf8(out).expect("utf-8 output"),
            format!(
                "installed moraebot-qa.service at {}\n{LINGER_HINT}\n",
                unit_file.display()
            )
        );
    }

    #[test]
    fn install_reports_systemctl_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fixture(&dir);
        let (fake, log) = fake_systemctl(dir.path(), 1);
        let units = dir.path().join("units");
        let err = install_with(
            "linux",
            &program(&fake),
            &exe(),
            &config,
            &units,
            UNIT,
            false,
            &mut Vec::new(),
        )
        .expect_err("systemctl fails");
        assert_eq!(err.code, E_SYSTEMCTL);
        assert_eq!(logged(&log), ["--user daemon-reload"]);
    }

    #[test]
    fn uninstall_disables_then_removes_the_unit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        fs::create_dir_all(&units).expect("mkdir units");
        let unit_file = units.join("moraebot-qa.service");
        fs::write(&unit_file, "[Unit]\n").expect("write unit");
        let mut out = Vec::new();
        uninstall_with("linux", &program(&fake), &units, UNIT, &mut out).expect("uninstalls");
        assert!(!unit_file.exists());
        assert_eq!(
            logged(&log),
            [
                "--user disable --now moraebot-qa.service",
                "--user daemon-reload"
            ]
        );
        assert_eq!(
            String::from_utf8(out).expect("utf-8 output"),
            "removed moraebot-qa.service\n"
        );
    }

    #[test]
    fn uninstall_without_unit_file_only_reloads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        uninstall_with("linux", &program(&fake), &units, UNIT, &mut Vec::new())
            .expect("nothing to remove is fine");
        assert_eq!(logged(&log), ["--user daemon-reload"]);
    }

    #[test]
    fn uninstall_refuses_off_linux() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (fake, log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        fs::create_dir_all(&units).expect("mkdir units");
        let unit_file = units.join("bad unit.service");
        fs::write(&unit_file, "[Unit]\n").expect("write unit");
        let err = uninstall_with(
            "macos",
            &program(&fake),
            &units,
            "bad unit",
            &mut Vec::new(),
        )
        .expect_err("refuses off Linux");
        assert_eq!(err.code, E_SERVICE_PLATFORM);
        assert!(unit_file.exists());
        assert_eq!(logged(&log), Vec::<String>::new());
        let reach = uninstall_with(
            "linux",
            &program(&fake),
            &units,
            "bad unit",
            &mut Vec::new(),
        )
        .expect_err("reaches the name check");
        assert_eq!(reach.code, E_LABEL_INVALID);
        assert_eq!(logged(&log), Vec::<String>::new());
    }

    #[test]
    fn rejects_bad_unit_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fixture(&dir);
        let (fake, _log) = fake_systemctl(dir.path(), 0);
        let units = dir.path().join("units");
        for bad in ["", "moraebot qa", "moraebot/qa", "moraebot;rm"] {
            let err = install_with(
                "linux",
                &program(&fake),
                &exe(),
                &config,
                &units,
                bad,
                true,
                &mut Vec::new(),
            )
            .expect_err("bad unit name");
            assert_eq!(err.code, E_LABEL_INVALID, "{bad}");
        }
        install_with(
            "linux",
            &program(&fake),
            &exe(),
            &config,
            &units,
            DEFAULT_UNIT,
            true,
            &mut Vec::new(),
        )
        .expect("default unit name is valid");
    }
}
