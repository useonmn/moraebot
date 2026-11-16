use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, ConfigError, Env, E_HOME_UNKNOWN, E_IO, E_OMP_MISSING};

pub const DEFAULT_LABEL: &str = "dev.moraebot.gateway";

pub const E_LABEL_INVALID: &str = "E_LABEL_INVALID";
pub const E_PATH_NOT_UTF8: &str = "E_PATH_NOT_UTF8";
pub const E_PLIST_LINT: &str = "E_PLIST_LINT";
pub const E_LAUNCHCTL: &str = "E_LAUNCHCTL";

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
  <integer>30</integer>
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
    ))
}

/// launchd creates StandardOutPath/StandardErrorPath itself with 0644 and ignores
/// the plist Umask for them, so the log folder and files are prepared here first.
fn prepare_log_files(home: &Path) -> Result<(), ConfigError> {
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
    check_label(label)?;
    config.validate()?;
    let domain = format!("gui/{}", current_uid()?);
    let plist = render_plist(label, &config.home, &config.omp_path)?;
    let plist_file = launch_agents_dir(env)?.join(format!("{label}.plist"));
    let plist_text = utf8(&plist_file)?;
    if dry_run {
        write!(out, "{plist}")?;
        writeln!(out, "launchctl bootstrap {domain} {plist_text}")?;
        writeln!(out, "launchctl bootout {domain}/{label}")?;
        return Ok(());
    }

    let bin_dir = config.home.join("bin");
    let binary = bin_dir.join("moraebot");
    let source = std::env::current_exe()
        .map_err(|e| ConfigError::io(E_IO, "cannot find the running moraebot binary", &e))?;
    fs::create_dir_all(&bin_dir)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot create {}", bin_dir.display()), &e))?;
    fs::copy(&source, &binary)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot copy to {}", binary.display()), &e))?;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
        .map_err(|e| ConfigError::io(E_IO, "cannot make the binary executable", &e))?;
    fs::create_dir_all(launch_agents_dir(env)?)
        .map_err(|e| ConfigError::io(E_IO, "cannot create the LaunchAgents folder", &e))?;
    prepare_log_files(&config.home)?;
    fs::write(&plist_file, &plist)
        .map_err(|e| ConfigError::io(E_IO, &format!("cannot write {plist_text}"), &e))?;

    run_command("plutil", &["-lint", plist_text], E_PLIST_LINT)?;
    run_command(
        "launchctl",
        &["bootstrap", domain.as_str(), plist_text],
        E_LAUNCHCTL,
    )?;
    writeln!(out, "installed {label} at {plist_text}")?;
    Ok(())
}

pub fn uninstall(env: &Env, label: &str, out: &mut impl Write) -> Result<(), ConfigError> {
    check_label(label)?;
    let target = format!("gui/{}/{label}", current_uid()?);
    run_command("launchctl", &["bootout", target.as_str()], E_LAUNCHCTL)?;
    let plist_file = launch_agents_dir(env)?.join(format!("{label}.plist"));
    fs::remove_file(&plist_file).map_err(|e| {
        ConfigError::io(E_IO, &format!("cannot remove {}", plist_file.display()), &e)
    })?;
    writeln!(out, "removed {label}")?;
    Ok(())
}

fn check_label(label: &str) -> Result<(), ConfigError> {
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

fn run_command(program: &str, args: &[&str], code: &'static str) -> Result<String, ConfigError> {
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

fn utf8(path: &Path) -> Result<&str, ConfigError> {
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

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
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
    fn dry_run_refuses_missing_omp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, env) = fixture(&dir);
        fs::remove_file(dir.path().join("omp")).expect("remove omp");
        let err = install(&config, &env, DEFAULT_LABEL, true, &mut Vec::new())
            .expect_err("refuses a missing omp");
        assert_eq!(err.code, E_OMP_MISSING);
    }
}
