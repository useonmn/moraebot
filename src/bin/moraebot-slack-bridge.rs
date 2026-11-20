//! `moraebot-slack-bridge`: holds the Slack connection for a moraebot core that runs
//! with `messenger = "bridge"` (see `docs/bridge.md`).

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use moraebot::config::{Env, report_checks};
use moraebot::logging;
use moraebot::slack_bridge::config::{self, SlackBridgeConfig};
use moraebot::slack_bridge::{self, BRIDGE_NAME, LOG_PREFIX};

const USAGE: &str = "\
Usage: moraebot-slack-bridge <command>

Commands:
  run [--config PATH]           Run the bridge until SIGTERM or SIGINT
  check-config [--config PATH]  Check the bridge config and its token files
  --version, -V                 Print the version

The config defaults to <moraebot home>/slack-bridge.toml.
";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run { config: Option<PathBuf> },
    CheckConfig { config: Option<PathBuf> },
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let (name, rest) = args.split_first().ok_or("missing command")?;
    let config = match rest {
        [] => None,
        [flag, path] if flag == "--config" => Some(PathBuf::from(path)),
        _ => return Err(format!("unexpected arguments for `{name}`")),
    };
    match name.as_str() {
        "run" => Ok(Command::Run { config }),
        "check-config" => Ok(Command::CheckConfig { config }),
        other => Err(format!("unknown command `{other}`")),
    }
}

fn config_path(env: &Env, config: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(match config {
        Some(path) => path,
        None => config::default_path(env)?,
    })
}

fn check_config(
    env: &Env,
    config: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let path = config_path(env, config)?;
    let mut stdout = io::stdout().lock();
    let config = match SlackBridgeConfig::load(&path, env) {
        Ok(config) => config,
        Err(err) => {
            writeln!(stdout, "FAIL config: {err}")?;
            return Ok(ExitCode::FAILURE);
        }
    };
    writeln!(stdout, "OK   config: {}", path.display())?;
    let ok = report_checks(&config.checks(), &mut stdout)?;
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run(env: &Env, config: Option<PathBuf>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let path = config_path(env, config)?;
    let config = SlackBridgeConfig::load(&path, env)?;
    config.validate()?;
    let home = env.default_home()?;
    let _log_guard = logging::init(&home, LOG_PREFIX)?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "{BRIDGE_NAME} starting"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(slack_bridge::run(config, &home));
    // A stuck blocking task must not delay the exit.
    runtime.shutdown_background();
    if let Err(err) = &result {
        tracing::error!(code = err.code, error = %err, "bridge failed");
    }
    result?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    // The socket, the spool, the lock and the logs are owner-only.
    unsafe { libc::umask(0o077) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("{BRIDGE_NAME} {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let env = Env::from_process();
    let result = match command {
        Command::Run { config } => run(&env, config),
        Command::CheckConfig { config } => check_config(&env, config),
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
    fn parses_the_commands_and_the_config_flag() {
        assert_eq!(
            parse_args(&args(&["run"])),
            Ok(Command::Run { config: None })
        );
        assert_eq!(
            parse_args(&args(&["check-config", "--config", "/x.toml"])),
            Ok(Command::CheckConfig {
                config: Some(PathBuf::from("/x.toml"))
            })
        );
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["serve"])).is_err());
        assert!(parse_args(&args(&["run", "--config"])).is_err());
        assert!(parse_args(&args(&["run", "--label", "x"])).is_err());
    }
}
