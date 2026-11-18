use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use moraebot::adapter::MessengerAdapter;
use moraebot::config::{Config, ConfigError, Env, Platform, report_checks};
use moraebot::discord::DiscordAdapter;
use moraebot::gateway::{E_SHUTDOWN_TIMEOUT, Gateway, GatewayError};
use moraebot::init::{self, InitOutcome};
use moraebot::launchd::{self, DEFAULT_LABEL};
use moraebot::logging;
use moraebot::onboarding;
use moraebot::slack::SlackAdapter;
use moraebot::systemd;
use tokio::signal::unix::{SignalKind, signal};

/// Time the gateway gets to stop after a signal; launchd's ExitTimeOut and systemd's TimeoutStopSec are 30 s.
const SHUTDOWN_BOUND: Duration = Duration::from_secs(20);
/// Extra time for closing the messenger connection after the gateway stopped.
const ADAPTER_SHUTDOWN_BOUND: Duration = Duration::from_secs(4);

const USAGE: &str = "\
Usage: moraebot <command>

Commands:
  init                          Ask a few questions and write the config
  setup <discord|slack>         Re-run one messenger's questions and make it active
  guide <discord|slack>         Print the setup guide for a messenger
  check-config [--config PATH]  Check the config and the things it points at
  run [--config PATH]           Run the gateway until SIGTERM or SIGINT
  install-launchd [--config PATH] [--label LABEL] [--dry-run]
                                Install the launchd agent (macOS)
  uninstall-launchd [--label LABEL]
                                Remove the launchd agent
  install-systemd [--config PATH] [--unit NAME] [--dry-run]
                                Install the systemd user service (Linux)
  uninstall-systemd [--unit NAME]
                                Remove the systemd user service
  onboarding [--config PATH]    Greet and ask the first-meeting questions again at the next start
  --version, -V                 Print the version
";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    CheckConfig {
        config: Option<PathBuf>,
    },
    Init,
    Setup {
        messenger: Platform,
    },
    Guide {
        messenger: Platform,
    },
    Run {
        config: Option<PathBuf>,
    },
    InstallLaunchd {
        config: Option<PathBuf>,
        label: String,
        dry_run: bool,
    },
    UninstallLaunchd {
        label: String,
    },
    InstallSystemd {
        config: Option<PathBuf>,
        unit: String,
        dry_run: bool,
    },
    UninstallSystemd {
        unit: String,
    },
    Onboarding {
        config: Option<PathBuf>,
    },
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some((name, rest)) = args.split_first() else {
        return Err("missing command".to_string());
    };
    match name.as_str() {
        "check-config" => parse_config_flag(rest).map(|config| Command::CheckConfig { config }),
        "run" => parse_config_flag(rest).map(|config| Command::Run { config }),
        "init" if rest.is_empty() => Ok(Command::Init),
        "init" => Err("init takes no arguments".to_string()),
        "setup" => parse_messenger(name, rest).map(|messenger| Command::Setup { messenger }),
        "guide" => parse_messenger(name, rest).map(|messenger| Command::Guide { messenger }),
        "install-launchd" => {
            parse_service_install(rest, "--label", DEFAULT_LABEL).map(|(config, label, dry_run)| {
                Command::InstallLaunchd {
                    config,
                    label,
                    dry_run,
                }
            })
        }
        "uninstall-launchd" => parse_service_name(rest, "--label", DEFAULT_LABEL)
            .map(|label| Command::UninstallLaunchd { label }),
        "install-systemd" => parse_service_install(rest, "--unit", systemd::DEFAULT_UNIT).map(
            |(config, unit, dry_run)| Command::InstallSystemd {
                config,
                unit,
                dry_run,
            },
        ),
        "uninstall-systemd" => parse_service_name(rest, "--unit", systemd::DEFAULT_UNIT)
            .map(|unit| Command::UninstallSystemd { unit }),
        "onboarding" => parse_config_flag(rest).map(|config| Command::Onboarding { config }),
        other => Err(format!("unknown command '{other}'")),
    }
}

fn parse_messenger(command: &str, args: &[String]) -> Result<Platform, String> {
    match args {
        [name] if name == "discord" => Ok(Platform::Discord),
        [name] if name == "slack" => Ok(Platform::Slack),
        [name] => Err(format!("unknown messenger '{name}'; use discord or slack")),
        _ => Err(format!("{command} needs one messenger: discord or slack")),
    }
}

fn parse_config_flag(args: &[String]) -> Result<Option<PathBuf>, String> {
    match args {
        [] => Ok(None),
        [flag, path] if flag == "--config" => Ok(Some(PathBuf::from(path))),
        [flag] if flag == "--config" => Err("--config needs a path".to_string()),
        [other, ..] => Err(format!("unexpected argument '{other}'")),
    }
}

/// `[--config PATH] [<name_flag> NAME] [--dry-run]` of a service install command:
/// the config path, the service name (`default` when not given) and the dry-run switch.
fn parse_service_install(
    args: &[String],
    name_flag: &str,
    default: &str,
) -> Result<(Option<PathBuf>, String, bool), String> {
    let mut config = None;
    let mut name = default.to_string();
    let mut dry_run = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(flag_value(&mut rest, arg)?)),
            flag if flag == name_flag => name = flag_value(&mut rest, arg)?,
            "--dry-run" => dry_run = true,
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    Ok((config, name, dry_run))
}

/// `[<name_flag> NAME]` of a service uninstall command: the service name, `default` when not given.
fn parse_service_name(args: &[String], name_flag: &str, default: &str) -> Result<String, String> {
    match args {
        [] => Ok(default.to_string()),
        [flag, name] if flag == name_flag => Ok(name.clone()),
        [flag] if flag == name_flag => Err(format!("{name_flag} needs a value")),
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

fn run_setup(env: &Env, messenger: Platform) -> Result<ExitCode, ConfigError> {
    let stdin = io::stdin();
    let is_tty = stdin.is_terminal();
    let outcome = init::setup(env, messenger, stdin.lock(), io::stdout().lock(), is_tty)?;
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

/// `<config>/systemd/user`, with `<config>` from `XDG_CONFIG_HOME` when it is absolute.
fn systemd_unit_dir(env: &Env) -> Result<PathBuf, ConfigError> {
    systemd::unit_dir(
        env,
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .as_deref(),
    )
}

fn install_systemd(
    env: &Env,
    config: Option<PathBuf>,
    unit: &str,
    dry_run: bool,
) -> Result<ExitCode, ConfigError> {
    let path = match config {
        Some(path) => path,
        None => env.default_config_path()?,
    };
    let config = Config::load(&path, env)?;
    systemd::install(
        &config,
        &systemd_unit_dir(env)?,
        unit,
        dry_run,
        &mut io::stdout().lock(),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn uninstall_systemd(env: &Env, unit: &str) -> Result<ExitCode, ConfigError> {
    systemd::uninstall(&systemd_unit_dir(env)?, unit, &mut io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

/// Asks for the first meeting with the owner at the next gateway start by creating the request
/// file. It never opens the store, so it works while a gateway holds it.
fn onboarding_request(env: &Env, config: Option<PathBuf>) -> Result<ExitCode, ConfigError> {
    let path = match config {
        Some(path) => path,
        None => env.default_config_path()?,
    };
    let config = Config::load(&path, env)?;
    if let Err(e) = onboarding::request(&config.home, onboarding::GreetTarget::Dm) {
        eprintln!("{}: {}", e.code, e.detail);
        return Ok(ExitCode::FAILURE);
    }
    writeln!(
        io::stdout().lock(),
        "{}",
        serde_json::json!({"onboarding": "requested"})
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Loads and validates the config, starts logging, then serves until a signal.
fn run_gateway(env: &Env, config: Option<PathBuf>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let path = match config {
        Some(path) => path,
        None => env.default_config_path()?,
    };
    let config = Config::load(&path, env)?;
    config.validate()?;
    let _log_guard = logging::init(&config.home)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve(config));
    // A stuck blocking task must not delay the exit.
    runtime.shutdown_background();
    if let Err(err) = &result {
        tracing::error!(code = err.code(), error = %err, "gateway failed");
    }
    result?;
    Ok(ExitCode::SUCCESS)
}

async fn serve(config: Config) -> Result<(), GatewayError> {
    let adapter: Arc<dyn MessengerAdapter> = match config.messenger {
        Platform::Discord => Arc::new(DiscordAdapter::from_config(&config)?),
        Platform::Slack => Arc::new(SlackAdapter::from_config(&config)?),
    };
    tracing::info!(platform = adapter.platform(), "starting messenger");
    let gateway = Gateway::open(&config, Arc::clone(&adapter))?;

    let mut terminate = signal(SignalKind::terminate()).map_err(GatewayError::Signal)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(GatewayError::Signal)?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let run = gateway.run(async {
        let _ = stopped.await;
    });
    tokio::pin!(run);

    let result = tokio::select! {
        result = &mut run => result,
        _ = terminate.recv() => stop_gateway(stop, run).await,
        _ = interrupt.recv() => stop_gateway(stop, run).await,
    };
    if tokio::time::timeout(ADAPTER_SHUTDOWN_BOUND, adapter.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            platform = adapter.platform(),
            "adapter shutdown did not finish in time"
        );
    }
    result
}

async fn stop_gateway(
    stop: tokio::sync::oneshot::Sender<()>,
    run: impl std::future::Future<Output = Result<(), GatewayError>>,
) -> Result<(), GatewayError> {
    tracing::info!("shutdown signal received");
    let _ = stop.send(());
    match tokio::time::timeout(SHUTDOWN_BOUND, run).await {
        Ok(result) => result,
        Err(_) => {
            tracing::error!(code = E_SHUTDOWN_TIMEOUT, "gateway did not stop in time");
            Err(GatewayError::ShutdownTimeout)
        }
    }
}

fn main() -> ExitCode {
    // Files this process creates (logs, launchd or systemd output) are owner-only.
    unsafe { libc::umask(0o077) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("moraebot {}", env!("CARGO_PKG_VERSION"));
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
        Command::CheckConfig { config } => check_config(&env, config),
        Command::Init => run_init(&env),
        Command::Setup { messenger } => run_setup(&env, messenger),
        Command::Guide { messenger } => {
            print!("{}", init::guide(messenger));
            Ok(ExitCode::SUCCESS)
        }
        Command::Run { config } => {
            return run_gateway(&env, config).unwrap_or_else(|err| {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            });
        }
        Command::InstallLaunchd {
            config,
            label,
            dry_run,
        } => install_launchd(&env, config, &label, dry_run),
        Command::UninstallLaunchd { label } => uninstall_launchd(&env, &label),
        Command::InstallSystemd {
            config,
            unit,
            dry_run,
        } => install_systemd(&env, config, &unit, dry_run),
        Command::UninstallSystemd { unit } => uninstall_systemd(&env, &unit),
        Command::Onboarding { config } => onboarding_request(&env, config),
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
            parse_args(&args(&["onboarding"])),
            Ok(Command::Onboarding { config: None })
        );
        assert_eq!(
            parse_args(&args(&["onboarding", "--config", "/x"])),
            Ok(Command::Onboarding {
                config: Some(PathBuf::from("/x"))
            })
        );
        assert_eq!(
            parse_args(&args(&["setup", "slack"])),
            Ok(Command::Setup {
                messenger: Platform::Slack
            })
        );
        assert_eq!(
            parse_args(&args(&["guide", "discord"])),
            Ok(Command::Guide {
                messenger: Platform::Discord
            })
        );
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
            parse_args(&args(&["install-systemd"])),
            Ok(Command::InstallSystemd {
                config: None,
                unit: "moraebot".to_string(),
                dry_run: false,
            })
        );
        assert_eq!(
            parse_args(&args(&[
                "install-systemd",
                "--dry-run",
                "--unit",
                "moraebot-qa",
                "--config",
                "x.toml"
            ])),
            Ok(Command::InstallSystemd {
                config: Some(PathBuf::from("x.toml")),
                unit: "moraebot-qa".to_string(),
                dry_run: true,
            })
        );
        assert_eq!(
            parse_args(&args(&["uninstall-systemd"])),
            Ok(Command::UninstallSystemd {
                unit: "moraebot".to_string(),
            })
        );
        assert_eq!(
            parse_args(&args(&["uninstall-systemd", "--unit", "moraebot-qa"])),
            Ok(Command::UninstallSystemd {
                unit: "moraebot-qa".to_string(),
            })
        );
        assert_eq!(
            parse_args(&args(&["run", "--config", "x.toml"])),
            Ok(Command::Run {
                config: Some(PathBuf::from("x.toml"))
            })
        );
        assert_eq!(
            parse_args(&args(&["run"])),
            Ok(Command::Run { config: None })
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
            &["run", "--config"],
            &["run", "extra"],
            &["init", "extra"],
            &["setup"],
            &["setup", "telegram"],
            &["guide", "slack", "extra"],
            &["check-config", "--config"],
            &["check-config", "--nope"],
            &["install-launchd", "--label"],
            &["install-launchd", "extra"],
            &["uninstall-launchd", "--dry-run"],
            &["install-systemd", "--unit"],
            &["install-systemd", "extra"],
            &["install-systemd", "--label", "x"],
            &["uninstall-systemd", "--dry-run"],
            &["uninstall-systemd", "--label", "x"],
            &["install-launchd", "--unit", "x"],
            &["uninstall-launchd", "--unit", "x"],
            &["onboarding", "extra"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
