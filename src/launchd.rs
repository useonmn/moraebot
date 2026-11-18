use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{Config, ConfigError, E_HOME_UNKNOWN, E_IO, E_OMP_MISSING, Env};

pub const DEFAULT_LABEL: &str = "dev.moraebot.gateway";

pub const E_LABEL_INVALID: &str = "E_LABEL_INVALID";
pub const E_PATH_NOT_UTF8: &str = "E_PATH_NOT_UTF8";
pub const E_PLIST_LINT: &str = "E_PLIST_LINT";
pub const E_LAUNCHCTL: &str = "E_LAUNCHCTL";
pub const E_SERVICE_PLATFORM: &str = "E_SERVICE_PLATFORM";

/// The plist `ExitTimeOut`; the reload wait is bounded by it so the two cannot drift apart.
pub(crate) const EXIT_TIMEOUT_SECS: u64 = 30;

/// `Ok` when `actual` (a `std::env::consts::OS` value) is `wanted`; `E_SERVICE_PLATFORM` otherwise.
pub(crate) fn require_os(actual: &str, wanted: &str) -> Result<(), ConfigError> {
    if actual == wanted {
        Ok(())
    } else {
        Err(ConfigError::new(
            E_SERVICE_PLATFORM,
            format!("this command needs {wanted}; this system is {actual}"),
        ))
    }
}

pub fn render_plist(label: &str, home: &Path, omp_path: &Path) -> Result<String, ConfigError> {
    check_label(label)?;
    let binary = home.join("bin").join("moraebot");
    let config = home.join("config.toml");
    let omp_dir = omp_path.parent().ok_or_else(|| {
        ConfigError::new(
            E_OMP_MISSING,
            format!("omp_path {} has no folder", omp_path.display()),
        )
    })?;
    let path_env = format!(
        "{}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        utf8(omp_dir)?
    );
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>run</string>
    <string>--config</string>
    <string>{config}</string>
  </array>
  <key>WorkingDirectory</key>
  <string>{home}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ExitTimeOut</key>
  <integer>{exit_timeout}</integer>
  <key>Umask</key>
  <integer>63</integer>
  <key>StandardOutPath</key>
  <string>{logs}/moraebot.out.log</string>
  <key>StandardErrorPath</key>
  <string>{logs}/moraebot.err.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path_env}</string>
    <key>MORAEBOT_HOME</key>
    <string>{home}</string>
  </dict>
</dict>
</plist>
"#,
        label = xml(label),
        binary = xml(utf8(&binary)?),
        config = xml(utf8(&config)?),
        home = xml(utf8(home)?),
        logs = xml(utf8(&home.join("logs"))?),
        path_env = xml(&path_env),
        exit_timeout = EXIT_TIMEOUT_SECS,
    ))
}

/// launchd creates StandardOutPath/StandardErrorPath itself with 0644 and ignores
/// the plist Umask for them, so the log folder and files are prepared here first.
pub(crate) fn prepare_log_files(home: &Path) -> Result<(), ConfigError> {
    let logs = home.join("logs");
    fs::create_dir_all(&logs)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot create {}", logs.display()), &e))?;
    fs::set_permissions(&logs, fs::Permissions::from_mode(0o700))
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot chmod {}", logs.display()), &e))?;
    for name in ["moraebot.out.log", "moraebot.err.log"] {
        let file = logs.join(name);
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&file)
            .map_err(|e| ConfigError::io(E_IO, &format!("cannot create {}", file.display()), &e))?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
            .map_err(|e| ConfigError::io(E_IO, &format!("cannot chmod {}", file.display()), &e))?;
    }
    Ok(())
}

pub fn install(
    config: &Config,
    env: &Env,
    label: &str,
    dry_run: bool,
    out: &mut impl Write,
) -> Result<(), ConfigError> {
    install_on(std::env::consts::OS, config, env, label, dry_run, out)
}

fn install_on(
    os: &str,
    config: &Config,
    env: &Env,
    label: &str,
    dry_run: bool,
    out: &mut impl Write,
) -> Result<(), ConfigError> {
    if !dry_run {
        require_os(os, "macos")?;
    }
    check_label(label)?;
    config.validate()?;
    let domain = format!("gui/{}", current_uid()?);
    let plist = render_plist(label, &config.home, &config.omp_path)?;
    let plist_file = launch_agents_dir(env)?.join(format!("{label}.plist"));
    let plist_text = utf8(&plist_file)?;
    if dry_run {
        write!(out, "{plist}")?;
        writeln!(out, "launchctl bootout {domain}/{label}")?;
        writeln!(out, "launchctl bootstrap {domain} {plist_text}")?;
        return Ok(());
    }

    let bin_dir = config.home.join("bin");
    let binary = bin_dir.join("moraebot");
    let source = std::env::current_exe()
        .map_err(|e| ConfigError::io(E_IO, "cannot find the running moraebot binary", &e))?;
    fs::create_dir_all(&bin_dir)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot create {}", bin_dir.display()), &e))?;
    install_binary(&source, &binary)?;
    fs::create_dir_all(launch_agents_dir(env)?)
        .map_err(|e| ConfigError::io(E_IO, "cannot create the LaunchAgents folder", &e))?;
    prepare_log_files(&config.home)?;
    fs::write(&plist_file, &plist)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot write {plist_text}"), &e))?;

    run_command("plutil", &["-lint", plist_text], E_PLIST_LINT)?;
    reload(&domain, label, plist_text)?;
    writeln!(out, "installed {label} at {plist_text}")?;
    Ok(())
}

/// Puts `source` at `binary` without ever copying a file onto itself (that truncates it)
/// and without leaving a half-written `binary`: copy to a temp file in the same folder,
/// then rename over the target.
pub(crate) fn install_binary(source: &Path, binary: &Path) -> Result<(), ConfigError> {
    let source = fs::canonicalize(source)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot resolve {}", source.display()), &e))?;
    let executable = fs::Permissions::from_mode(0o755);
    if fs::canonicalize(binary).is_ok_and(|existing| existing == source) {
        return fs::set_permissions(binary, executable)
            .map_err(|e| ConfigError::io(E_IO, &format!("cannot chmod {}", binary.display()), &e));
    }
    let bin_dir = binary
        .parent()
        .ok_or_else(|| ConfigError::new(E_IO, format!("{} has no folder", binary.display())))?;
    let temp = bin_dir.join(format!(".moraebot.new.{}", std::process::id()));
    let replaced = fs::copy(&source, &temp)
        .and_then(|_| fs::set_permissions(&temp, executable))
        .and_then(|()| fs::rename(&temp, binary));
    if let Err(e) = replaced {
        if let Err(cleanup) = fs::remove_file(&temp)
            && cleanup.kind() != std::io::ErrorKind::NotFound
        {
            return Err(ConfigError::new(
                E_IO,
                format!(
                    "cannot replace {}: {e}; also cannot remove {}: {cleanup}",
                    binary.display(),
                    temp.display()
                ),
            ));
        }
        return Err(ConfigError::io(
            E_IO,
            &format!("cannot replace {}", binary.display()),
            &e,
        ));
    }
    Ok(())
}

/// Boots out an already loaded job (ignoring "not loaded"), waits until launchd has
/// really dropped it, then bootstraps the new plist, so a re-run upgrades and restarts.
fn reload(domain: &str, label: &str, plist_text: &str) -> Result<(), ConfigError> {
    let retry = format!("run `moraebot uninstall-launchd --label {label}` and retry");
    bootout_and_wait(domain, label, &retry)?;
    run_command("launchctl", &["bootstrap", domain, plist_text], E_LAUNCHCTL)
        .map_err(|e| ConfigError::new(E_LAUNCHCTL, format!("{}; {retry}", e.message)))?;
    Ok(())
}

/// Boots out `domain/label`, ignoring the exit status (a job that is not loaded is
/// fine), then waits until `launchctl print` no longer finds it, bounded by the
/// plist `ExitTimeOut` plus a margin.
fn bootout_and_wait(domain: &str, label: &str, hint: &str) -> Result<(), ConfigError> {
    let target = format!("{domain}/{label}");
    Command::new("launchctl")
        .args(["bootout", target.as_str()])
        .output()
        .map_err(|e| ConfigError::io(E_LAUNCHCTL, "cannot run launchctl", &e))?;
    let deadline = Instant::now() + Duration::from_secs(EXIT_TIMEOUT_SECS + 5);
    loop {
        let loaded = Command::new("launchctl")
            .args(["print", target.as_str()])
            .output()
            .map_err(|e| ConfigError::io(E_LAUNCHCTL, "cannot run launchctl", &e))?
            .status
            .success();
        if !loaded {
            break;
        }
        if Instant::now() >= deadline {
            return Err(ConfigError::new(
                E_LAUNCHCTL,
                format!(
                    "{target} is still loaded after {}s; {hint}",
                    EXIT_TIMEOUT_SECS + 5
                ),
            ));
        }
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// Unloads the job (fine when it is not loaded) and removes its plist (fine when
/// it is already gone); returns only after launchd has really dropped the job.
pub fn uninstall(env: &Env, label: &str, out: &mut impl Write) -> Result<(), ConfigError> {
    uninstall_on(std::env::consts::OS, env, label, out)
}

fn uninstall_on(os: &str, env: &Env, label: &str, out: &mut impl Write) -> Result<(), ConfigError> {
    require_os(os, "macos")?;
    check_label(label)?;
    let domain = format!("gui/{}", current_uid()?);
    bootout_and_wait(&domain, label, "retry `moraebot uninstall-launchd`")?;
    remove_plist(&launch_agents_dir(env)?.join(format!("{label}.plist")))?;
    writeln!(out, "removed {label}")?;
    Ok(())
}

pub(crate) fn remove_plist(plist_file: &Path) -> Result<(), ConfigError> {
    match fs::remove_file(plist_file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(ConfigError::io(
            E_IO,
            &format!("cannot remove {}", plist_file.display()),
            &e,
        )),
        _ => Ok(()),
    }
}

pub(crate) fn check_label(label: &str) -> Result<(), ConfigError> {
    let valid = !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(ConfigError::new(
            E_LABEL_INVALID,
            "label may contain only letters, digits, '.' and '-'",
        ))
    }
}

fn launch_agents_dir(env: &Env) -> Result<PathBuf, ConfigError> {
    let home = env.home_dir.as_ref().ok_or_else(|| {
        ConfigError::new(
            E_HOME_UNKNOWN,
            "HOME is not set, cannot find ~/Library/LaunchAgents",
        )
    })?;
    Ok(home.join("Library").join("LaunchAgents"))
}

fn current_uid() -> Result<String, ConfigError> {
    run_command("id", &["-u"], E_LAUNCHCTL)
}

pub(crate) fn run_command(
    program: &str,
    args: &[&str],
    code: &'static str,
) -> Result<String, ConfigError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| ConfigError::io(code, &format!("cannot run {program}"), &e))?;
    if !output.status.success() {
        return Err(ConfigError::new(
            code,
            format!(
                "{program} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn utf8(path: &Path) -> Result<&str, ConfigError> {
    path.to_str().ok_or_else(|| {
        ConfigError::new(
            E_PATH_NOT_UTF8,
            format!("path is not valid UTF-8: {}", path.display()),
        )
    })
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const GOLDEN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.moraebot.gateway</string>
  <key>ProgramArguments</key>
  <array>
    <string>/tmp/moraebot test home/bin/moraebot</string>
    <string>run</string>
    <string>--config</string>
    <string>/tmp/moraebot test home/config.toml</string>
  </array>
  <key>WorkingDirectory</key>
  <string>/tmp/moraebot test home</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ExitTimeOut</key>
  <integer>30</integer>
  <key>Umask</key>
  <integer>63</integer>
  <key>StandardOutPath</key>
  <string>/tmp/moraebot test home/logs/moraebot.out.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/moraebot test home/logs/moraebot.err.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/tmp/moraebot test omp/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>MORAEBOT_HOME</key>
    <string>/tmp/moraebot test home</string>
  </dict>
</dict>
</plist>
"#;

    fn fixture(dir: &TempDir) -> (Config, Env) {
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
            dir.path().join("home").display(),
            dir.path().join("work").display(),
        );
        let config = Config::from_toml(&text, &env).expect("loads");
        (config, env)
    }

    #[test]
    fn renders_golden_plist() {
        let plist = render_plist(
            DEFAULT_LABEL,
            Path::new("/tmp/moraebot test home"),
            Path::new("/tmp/moraebot test omp/bin/omp"),
        )
        .expect("renders");
        assert_eq!(plist, GOLDEN);
    }

    #[test]
    fn escapes_xml_in_paths() {
        let plist = render_plist(
            DEFAULT_LABEL,
            Path::new("/tmp/a&b home"),
            Path::new("/tmp/omp dir/omp"),
        )
        .expect("renders");
        assert!(plist.contains("<string>/tmp/a&amp;b home/bin/moraebot</string>"));
    }

    #[test]
    fn rejects_bad_labels() {
        for bad in ["", "dev moraebot", "dev/moraebot", "dev.moraebot;rm"] {
            assert_eq!(
                check_label(bad).expect_err("bad label").code,
                E_LABEL_INVALID,
                "{bad}"
            );
        }
        check_label(DEFAULT_LABEL).expect("default label is valid");
    }

    #[test]
    fn dry_run_prints_plist_and_launchctl_commands_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, env) = fixture(&dir);
        let mut out = Vec::new();
        install(&config, &env, DEFAULT_LABEL, true, &mut out).expect("dry run");
        let text = String::from_utf8(out).expect("utf-8 output");
        assert!(text.starts_with("<?xml"));
        assert!(text.contains("launchctl bootstrap gui/"));
        assert!(text.contains("launchctl bootout gui/"));
        assert!(
            !dir.path().join("Library").exists(),
            "dry run must not write"
        );
        let logs = config.home.join("logs");
        assert!(
            !logs.join("moraebot.out.log").exists() && !logs.join("moraebot.err.log").exists(),
            "dry run must not create log files"
        );
    }

    #[test]
    fn dry_run_prints_bootout_before_bootstrap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, env) = fixture(&dir);
        let mut out = Vec::new();
        install(&config, &env, DEFAULT_LABEL, true, &mut out).expect("dry run");
        let text = String::from_utf8(out).expect("utf-8 output");
        let bootout = text.find("launchctl bootout ").expect("bootout line");
        let bootstrap = text.find("launchctl bootstrap ").expect("bootstrap line");
        assert!(bootout < bootstrap, "bootout must come first:\n{text}");
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    fn leftover_temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(".moraebot.new."))
            .collect()
    }

    fn write_file(path: &Path, bytes: &[u8], mode: u32) {
        fs::write(path, bytes).expect("write file");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod file");
    }

    #[test]
    fn install_binary_creates_a_missing_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        write_file(&source, b"new binary", 0o700);
        let bin_dir = dir.path().join("bin");
        fs::create_dir(&bin_dir).expect("mkdir bin");
        let binary = bin_dir.join("moraebot");
        install_binary(&source, &binary).expect("installs");
        assert_eq!(fs::read(&binary).expect("read binary"), b"new binary");
        assert_eq!(mode_of(&binary), 0o755);
        assert_eq!(leftover_temp_files(&bin_dir), Vec::<String>::new());
    }

    #[test]
    fn install_binary_replaces_a_different_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        write_file(&source, b"new binary", 0o755);
        let bin_dir = dir.path().join("bin");
        fs::create_dir(&bin_dir).expect("mkdir bin");
        let binary = bin_dir.join("moraebot");
        write_file(&binary, b"old binary, longer than the new one", 0o755);
        install_binary(&source, &binary).expect("replaces");
        assert_eq!(fs::read(&binary).expect("read binary"), b"new binary");
        assert_eq!(mode_of(&binary), 0o755);
        assert_eq!(leftover_temp_files(&bin_dir), Vec::<String>::new());
    }

    #[test]
    fn install_binary_keeps_the_running_binary_when_source_is_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        fs::create_dir(&bin_dir).expect("mkdir bin");
        let binary = bin_dir.join("moraebot");
        write_file(&binary, b"running binary", 0o644);
        install_binary(&binary, &binary).expect("keeps");
        assert_eq!(fs::read(&binary).expect("read binary"), b"running binary");
        assert_eq!(mode_of(&binary), 0o755);
        assert_eq!(leftover_temp_files(&bin_dir), Vec::<String>::new());
    }

    #[test]
    fn prepare_log_files_creates_owner_only_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        prepare_log_files(&home).expect("prepares");
        assert_eq!(mode_of(&home.join("logs")), 0o700);
        assert_eq!(mode_of(&home.join("logs").join("moraebot.out.log")), 0o600);
        assert_eq!(mode_of(&home.join("logs").join("moraebot.err.log")), 0o600);
    }

    #[test]
    fn prepare_log_files_tightens_existing_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs");
        fs::create_dir(&logs).expect("mkdir logs");
        let out = logs.join("moraebot.out.log");
        fs::write(&out, "kept\n").expect("write out log");
        fs::set_permissions(&out, fs::Permissions::from_mode(0o644)).expect("chmod out log");
        prepare_log_files(dir.path()).expect("prepares");
        assert_eq!(mode_of(&out), 0o600);
        assert_eq!(fs::read_to_string(&out).expect("read out log"), "kept\n");
        assert_eq!(mode_of(&logs.join("moraebot.err.log")), 0o600);
    }

    #[test]
    fn remove_plist_deletes_an_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plist = dir.path().join("dev.moraebot.gateway.test.plist");
        fs::write(&plist, "<plist/>").expect("write plist");
        remove_plist(&plist).expect("removes");
        assert!(!plist.exists());
    }

    #[test]
    fn remove_plist_accepts_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        remove_plist(&dir.path().join("missing.plist")).expect("missing plist is fine");
    }

    #[test]
    fn remove_plist_reports_other_errors_as_io() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_file = dir.path().join("folder.plist");
        fs::create_dir(&not_a_file).expect("mkdir");
        let err = remove_plist(&not_a_file).expect_err("a folder is not removable as a file");
        assert_eq!(err.code, E_IO);
    }

    #[test]
    fn dry_run_refuses_missing_omp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, env) = fixture(&dir);
        fs::remove_file(dir.path().join("omp")).expect("remove omp");
        let err = install(&config, &env, DEFAULT_LABEL, true, &mut Vec::new())
            .expect_err("refuses a missing omp");
        assert_eq!(err.code, E_OMP_MISSING);
    }

    #[test]
    fn require_os_matches_exactly() {
        require_os("macos", "macos").expect("same system");
        for (actual, wanted) in [("linux", "macos"), ("macos", "linux")] {
            assert_eq!(
                require_os(actual, wanted).expect_err("other system").code,
                E_SERVICE_PLATFORM,
                "{actual} {wanted}"
            );
        }
    }

    #[test]
    fn install_refuses_off_macos() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, env) = fixture(&dir);
        let mut out = Vec::new();
        let err = install_on("linux", &config, &env, "bad label", false, &mut out)
            .expect_err("refuses off macOS");
        assert_eq!(err.code, E_SERVICE_PLATFORM);
        assert!(out.is_empty());
        let reach = install_on("linux", &config, &env, "bad label", true, &mut Vec::new())
            .expect_err("dry run reaches the label check");
        assert_eq!(reach.code, E_LABEL_INVALID);
    }

    #[test]
    fn uninstall_refuses_off_macos() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_config, env) = fixture(&dir);
        let agents = dir.path().join("Library").join("LaunchAgents");
        fs::create_dir_all(&agents).expect("mkdir LaunchAgents");
        let plist = agents.join("bad label.plist");
        fs::write(&plist, "<plist/>").expect("write plist");
        let mut out = Vec::new();
        let err =
            uninstall_on("linux", &env, "bad label", &mut out).expect_err("refuses off macOS");
        assert_eq!(err.code, E_SERVICE_PLATFORM);
        assert!(plist.exists());
        assert_eq!(
            check_label("bad label").expect_err("bad label").code,
            E_LABEL_INVALID
        );
    }
}
