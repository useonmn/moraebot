use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use moraebot::config::{report_checks, Config, ConfigError, Env};
use moraebot::init::{self, InitOutcome};

const USAGE: &str = "\
Usage: moraebot <command>

Commands:
  init                          Ask a few questions and write the config
  check-config [--config PATH]  Check the config and the things it points at
";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    CheckConfig { config: Option<PathBuf> },
    Init,
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some((name, rest)) = args.split_first() else {
        return Err("missing command".to_string());
    };
    // New subcommands (run, install-launchd, uninstall-launchd) get an arm here.
    match name.as_str() {
        "check-config" => parse_check_config(rest),
        "init" if rest.is_empty() => Ok(Command::Init),
        "init" => Err("init takes no arguments".to_string()),
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
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
