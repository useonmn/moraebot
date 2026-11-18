//! The Linux background service: a systemd --user unit that keeps `moraebot run` going,
//! the counterpart of `launchd` on macOS.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, ConfigError, E_HOME_UNKNOWN, E_IO, E_OMP_MISSING, Env};
use crate::launchd;

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

/// The unit text for `<home>/bin/moraebot run --config <home>/config.toml`.
pub fn render_unit(home: &Path, omp_path: &Path) -> Result<String, ConfigError> {
    let omp_dir = omp_path.parent().ok_or_else(|| {
        ConfigError::new(
            E_OMP_MISSING,
            format!("omp_path {} has no folder", omp_path.display()),
        )
    })?;
    let home = unit_path(home)?;
    let omp_dir = unit_path(omp_dir)?;
    Ok(format!(
        r#"[Unit]
Description=moraebot gateway

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

/// Installs (or upgrades and restarts) the user service named `unit` (`<unit>.service` in `unit_dir`).
pub fn install(
    config: &Config,
    unit_dir: &Path,
    unit: &str,
    dry_run: bool,
    out: &mut impl Write,
) -> Result<(), ConfigError> {
    install_with(
        std::env::consts::OS,
        &[OsStr::new(SYSTEMCTL)],
        config,
        unit_dir,
        unit,
        dry_run,
        out,
    )
}

/// Stops and removes the user service named `unit`; fine when it is not installed.
pub fn uninstall(unit_dir: &Path, unit: &str, out: &mut impl Write) -> Result<(), ConfigError> {
    uninstall_with(
        std::env::consts::OS,
        &[OsStr::new(SYSTEMCTL)],
        unit_dir,
        unit,
        out,
    )
}

fn install_with(
    os: &str,
    systemctl: &[&OsStr],
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
    let text = render_unit(&config.home, &config.omp_path)?;
    let service = format!("{unit}.service");
    let unit_file = unit_dir.join(&service);
    let file_text = unit_path(&unit_file)?;
    if dry_run {
        writeln!(out, "# {file_text}")?;
        write!(out, "{text}")?;
        writeln!(out, "systemctl --user daemon-reload")?;
        writeln!(out, "systemctl --user enable {service}")?;
        writeln!(out, "systemctl --user restart {service}")?;
        return Ok(());
    }

    let bin_dir = config.home.join("bin");
    let source = std::env::current_exe()
        .map_err(|e| ConfigError::io(E_IO, "cannot find the running moraebot binary", &e))?;
    fs::create_dir_all(&bin_dir)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot create {}", bin_dir.display()), &e))?;
    launchd::install_binary(&source, &bin_dir.join("moraebot"))?;
    fs::create_dir_all(unit_dir)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot create {}", unit_dir.display()), &e))?;
    launchd::prepare_log_files(&config.home)?;
    fs::write(&unit_file, &text)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot write {file_text}"), &e))?;

    systemctl_user(systemctl, &["daemon-reload"])?;
    systemctl_user(systemctl, &["enable", &service])?;
    systemctl_user(systemctl, &["restart", &service])?;
    writeln!(out, "installed {service} at {file_text}")?;
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
    let unit_file = unit_dir.join(&service);
    if fs::symlink_metadata(&unit_file).is_ok() {
        systemctl_user(systemctl, &["disable", "--now", &service])?;
    }
    launchd::remove_plist(&unit_file)?;
    systemctl_user(systemctl, &["daemon-reload"])?;
    writeln!(out, "removed {service}")?;
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

    const UNIT: &str = "moraebot-qa";

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

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    #[test]
    fn renders_golden_unit() {
        let unit = render_unit(
            Path::new("/tmp/moraebot test home"),
            Path::new("/tmp/moraebot test omp/bin/omp"),
        )
        .expect("renders");
        assert_eq!(unit, GOLDEN_UNIT);
    }

    #[test]
    fn unit_refuses_unsafe_path_characters() {
        for c in ['"', '\\', '$', '%', '\n'] {
            let home = render_unit(
                &PathBuf::from(format!("/tmp/qa{c}mbhome")),
                Path::new("/tmp/omp/omp"),
            )
            .expect_err("unsafe home");
            assert_eq!(home.code, E_UNIT_PATH, "home {c:?}");
            let omp = render_unit(
                Path::new("/tmp/qa-mbhome"),
                &PathBuf::from(format!("/tmp/omp{c}dir/omp")),
            )
            .expect_err("unsafe omp dir");
            assert_eq!(omp.code, E_UNIT_PATH, "omp {c:?}");
        }
        render_unit(Path::new("/tmp/qa mbhome"), Path::new("/tmp/omp dir/omp"))
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
            render_unit(&config.home, &config.omp_path).expect("renders")
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
            render_unit(&config.home, &config.omp_path).expect("renders")
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
            &config,
            &units,
            DEFAULT_UNIT,
            true,
            &mut Vec::new(),
        )
        .expect("default unit name is valid");
    }
}
