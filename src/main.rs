use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use moraebot::config::{report_checks, Config, ConfigError, Env};
use moraebot::init::{self, InitOutcome};
use moraebot::launchd::{self, DEFAULT_LABEL};

const USAGE: &str = "\
Usage: moraebot <command>

Commands:
  init                          Ask a few questions and write the config
  check-config [--config PATH]  Check the config and the things it points at
  install-launchd [--config PATH] [--label LABEL] [--dry-run]
                                Install the launchd agent (macOS)
  uninstall-launchd [--label LABEL]
                                Remove the launchd agent
";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    CheckConfig {
        config: Option<PathBuf>,
    },
    Init,
    InstallLaunchd {
        config: Option<PathBuf>,
        label: String,
        dry_run: bool,
    },
    UninstallLaunchd {
        label: String,
    },
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some((name, rest)) = args.split_first() else {
        return Err("missing command".to_string());
    };
    match name.as_str() {
        "check-config" => parse_check_config(rest),
        "init" if rest.is_empty() => Ok(Command::Init),
        "init" => Err("init takes no arguments".to_string()),
        "install-launchd" => parse_install_launchd(rest),
        "uninstall-launchd" => parse_uninstall_launchd(rest),
        other => Err(format!("unknown command '{other}'")),
    }
}

fn parse_check_config(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::CheckConfig { config: None }),
        [flag, path] if flag == "--config" => Ok(Command::CheckConfig {
            config: Some(PathBuf::from(path)),
        }),
        [flag] if flag == "--config" => Err("--config needs a path".to_string()),
        [other, ..] => Err(format!("unexpected argument '{other}'")),
    }
}

fn parse_install_launchd(args: &[String]) -> Result<Command, String> {
    let mut config = None;
    let mut label = DEFAULT_LABEL.to_string();
    let mut dry_run = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(flag_value(&mut rest, arg)?)),
            "--label" => label = flag_value(&mut rest, arg)?,
            "--dry-run" => dry_run = true,
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    Ok(Command::InstallLaunchd {
        config,
        label,
        dry_run,
    })
}

fn parse_uninstall_launchd(args: &[String]) -> Result<Command, String> {
    match args {
        [] => Ok(Command::UninstallLaunchd {
            label: DEFAULT_LABEL.to_string(),
        }),
        [flag, label] if flag == "--label" => Ok(Command::UninstallLaunchd {
            label: label.clone(),
        }),
        [flag] if flag == "--label" => Err("--label needs a value".to_string()),
        [other, ..] => Err(format!("unexpected argument '{other}'")),
    }
}

fn flag_value(rest: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    rest.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn check_config(env: &Env, config: Option<PathBuf>) -> Result<ExitCode, ConfigError> {
    let path = match config {
        Some(path) => path,
        None => env.default_config_path()?,
    };
    let mut stdout = io::stdout().lock();
    let config = match Config::load(&path, env) {
        Ok(config) => config,
        Err(err) => {
            writeln!(stdout, "FAIL config: {err}")?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let all_ok = report_checks(&config.checks(), &mut stdout)?;
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run_init(env: &Env) -> Result<ExitCode, ConfigError> {
    let stdin = io::stdin();
    let is_tty = stdin.is_terminal();
    let outcome = init::run(env, stdin.lock(), io::stdout().lock(), is_tty)?;
    Ok(match outcome {
        InitOutcome::Written { checks_ok: false } => ExitCode::FAILURE,
        InitOutcome::Written { checks_ok: true } | InitOutcome::Kept => ExitCode::SUCCESS,
    })
}

fn install_launchd(
    env: &Env,
    config: Option<PathBuf>,
    label: &str,
    dry_run: bool,
) -> Result<ExitCode, ConfigError> {
    let path = match config {
        Some(path) => path,
        None => env.default_config_path()?,
    };
    let config = Config::load(&path, env)?;
    launchd::install(&config, env, label, dry_run, &mut io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn uninstall_launchd(env: &Env, label: &str) -> Result<ExitCode, ConfigError> {
    launchd::uninstall(env, label, &mut io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let env = Env::from_process();
    let result = match command {
        Command::CheckConfig { config } => check_config(&env, config),
        Command::Init => run_init(&env),
        Command::InstallLaunchd {
            config,
            label,
            dry_run,
        } => install_launchd(&env, config, &label, dry_run),
        Command::UninstallLaunchd { label } => uninstall_launchd(&env, &label),
    };
    result.unwrap_or_else(|err| {
        eprintln!("error: {err}");
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_known_commands() {
        assert_eq!(parse_args(&args(&["init"])), Ok(Command::Init));
        assert_eq!(
            parse_args(&args(&[
                "install-launchd",
                "--dry-run",
                "--config",
                "x.toml"
            ])),
            Ok(Command::InstallLaunchd {
                config: Some(PathBuf::from("x.toml")),
                label: DEFAULT_LABEL.to_string(),
                dry_run: true,
            })
        );
        assert_eq!(
            parse_args(&args(&[
                "uninstall-launchd",
                "--label",
                "dev.moraebot.gateway.qa"
            ])),
            Ok(Command::UninstallLaunchd {
                label: "dev.moraebot.gateway.qa".to_string(),
            })
        );
        assert_eq!(
            parse_args(&args(&["check-config"])),
            Ok(Command::CheckConfig { config: None })
        );
        assert_eq!(
            parse_args(&args(&["check-config", "--config", "x.toml"])),
            Ok(Command::CheckConfig {
                config: Some(PathBuf::from("x.toml"))
            })
        );
    }

    #[test]
    fn rejects_bad_usage() {
        for bad in [
            &[][..],
            &["bogus"],
            &["run"],
            &["init", "extra"],
            &["check-config", "--config"],
            &["check-config", "--nope"],
            &["install-launchd", "--label"],
            &["install-launchd", "extra"],
            &["uninstall-launchd", "--dry-run"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
