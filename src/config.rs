//! Configuration loading and validation.
//!
//! The config file never contains the bot token: it only points at a token file,
//! and the token content is never read into [`Config`].

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const E_MESSENGER: &str = "E_MESSENGER";
pub const E_MESSENGER_MISSING: &str = "E_MESSENGER_MISSING";
pub const E_OWNER_MISSING: &str = "E_OWNER_MISSING";
pub const E_CHANNEL_MISSING: &str = "E_CHANNEL_MISSING";
pub const E_ACCESS_MODE: &str = "E_ACCESS_MODE";
pub const E_TOKEN_MISSING: &str = "E_TOKEN_MISSING";
pub const E_TOKEN_NOT_FILE: &str = "E_TOKEN_NOT_FILE";
pub const E_TOKEN_MODE: &str = "E_TOKEN_MODE";
pub const E_OMP_MISSING: &str = "E_OMP_MISSING";
pub const E_WORK_DIR: &str = "E_WORK_DIR";
pub const E_HOME_UNKNOWN: &str = "E_HOME_UNKNOWN";
pub const E_HOME_LAYOUT: &str = "E_HOME_LAYOUT";
pub const E_CONFIG_READ: &str = "E_CONFIG_READ";
pub const E_CONFIG_PARSE: &str = "E_CONFIG_PARSE";
pub const E_CONFIG_VALUE: &str = "E_CONFIG_VALUE";
pub const E_IO: &str = "E_IO";

const DEFAULT_WORK_DIR: &str = "~/moraebot-work";

/// An error with a stable machine-readable code. Tests assert the code, never the prose.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub code: &'static str,
    pub message: String,
}

impl ConfigError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn io(code: &'static str, context: &str, err: &io::Error) -> Self {
        Self::new(code, format!("{context}: {err}"))
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(err: io::Error) -> Self {
        Self::new(E_IO, err.to_string())
    }
}

/// Process environment inputs, passed explicitly so tests never touch real variables.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// `$HOME`.
    pub home_dir: Option<PathBuf>,
    /// `$MORAEBOT_HOME`, overrides the moraebot home folder.
    pub moraebot_home: Option<PathBuf>,
    /// `$PATH`.
    pub path: Option<OsString>,
}

impl Env {
    pub fn from_process() -> Self {
        let nonempty = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty());
        Self {
            home_dir: nonempty("HOME").map(PathBuf::from),
            moraebot_home: nonempty("MORAEBOT_HOME").map(PathBuf::from),
            path: std::env::var_os("PATH"),
        }
    }

    /// `$MORAEBOT_HOME` if set, else `$HOME/.moraebot`.
    pub fn default_home(&self) -> Result<PathBuf, ConfigError> {
        if let Some(home) = &self.moraebot_home {
            return Ok(home.clone());
        }
        self.home_dir
            .as_ref()
            .map(|h| h.join(".moraebot"))
            .ok_or_else(|| {
                ConfigError::new(E_HOME_UNKNOWN, "HOME is not set; set MORAEBOT_HOME or HOME")
            })
    }

    pub fn default_config_path(&self) -> Result<PathBuf, ConfigError> {
        self.default_home().map(|home| home.join("config.toml"))
    }

    /// Expands a leading `~` using `$HOME`.
    pub fn expand_tilde(&self, path: &Path) -> Result<PathBuf, ConfigError> {
        let Ok(rest) = path.strip_prefix("~") else {
            return Ok(path.to_path_buf());
        };
        let home = self.home_dir.as_ref().ok_or_else(|| {
            ConfigError::new(E_HOME_UNKNOWN, "HOME is not set, cannot expand '~'")
        })?;
        Ok(home.join(rest))
    }
}

/// Only `owner_only` exists in slice 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    OwnerOnly,
}

impl AccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AccessMode::OwnerOnly => "owner_only",
        }
    }
}

/// The messenger the gateway talks to. Only one is active at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Discord,
    Slack,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Discord => "discord",
            Platform::Slack => "slack",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscordConfig {
    pub owner_id: u64,
    pub channel_id: u64,
    pub token_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SlackConfig {
    pub owner_id: String,
    pub channel_id: String,
    pub bot_token_file: PathBuf,
    pub app_token_file: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct OmpConfig {
    /// Absent means no turn time limit.
    pub turn_timeout_minutes: Option<u64>,
    /// Absent means no cost limit.
    pub max_cost_usd: Option<f64>,
}

#[derive(Clone)]
pub struct Config {
    pub messenger: Platform,
    pub discord: Option<DiscordConfig>,
    pub slack: Option<SlackConfig>,
    pub work_dir: PathBuf,
    pub omp_path: PathBuf,
    pub home: PathBuf,
    pub access_mode: AccessMode,
    pub omp: OmpConfig,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("messenger", &self.messenger)
            .field("discord", &self.discord)
            .field("slack", &self.slack)
            .field("work_dir", &self.work_dir)
            .field("omp_path", &self.omp_path)
            .field("home", &self.home)
            .field("access_mode", &self.access_mode)
            .field("omp", &self.omp)
            .finish()
    }
}

/// On-disk shape of `config.toml`. The top-level `owner_id` and `token_file` are the
/// slice-1 Discord layout: they are read but never written.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    messenger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    work_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    omp_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    home: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    access_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    discord: Option<DiscordSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slack: Option<SlackSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    omp: Option<OmpSection>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiscordSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SlackSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bot_token_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_token_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OmpSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_timeout_minutes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_cost_usd: Option<f64>,
}

/// One line of `check-config` output.
#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    /// `Ok(detail)` or the failure.
    pub result: Result<String, ConfigError>,
    /// A failing advisory check is reported as `WARN` and never fails validation.
    pub advisory: bool,
}

impl Config {
    pub fn load(path: &Path, env: &Env) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(path).map_err(|e| {
            ConfigError::io(
                E_CONFIG_READ,
                &format!("cannot read {}", path.display()),
                &e,
            )
        })?;
        Self::from_toml(&text, env)
    }

    /// Parses config text and resolves defaults. Filesystem checks live in [`Config::validate`].
    pub fn from_toml(text: &str, env: &Env) -> Result<Config, ConfigError> {
        let file: ConfigFile =
            toml::from_str(text).map_err(|e| ConfigError::new(E_CONFIG_PARSE, e.to_string()))?;

        let access_mode = match file.access_mode.as_deref() {
            None | Some("owner_only") => AccessMode::OwnerOnly,
            Some(other) => {
                return Err(ConfigError::new(
                    E_ACCESS_MODE,
                    format!(
                        "access_mode '{other}' is not available yet; other modes arrive in stage 2, \
                         only 'owner_only' is supported"
                    ),
                ));
            }
        };
        let messenger = match file.messenger.as_deref() {
            None | Some("discord") => Platform::Discord,
            Some("slack") => Platform::Slack,
            Some(other) => {
                return Err(ConfigError::new(
                    E_MESSENGER,
                    format!("messenger '{other}' is not available; use 'discord' or 'slack'"),
                ));
            }
        };
        let discord = discord_config(file.discord, file.owner_id, file.token_file, env)?;
        let slack = slack_config(file.slack, env)?;
        let missing = match messenger {
            Platform::Discord => discord.is_none().then_some("[discord]"),
            Platform::Slack => slack.is_none().then_some("[slack]"),
        };
        if let Some(section) = missing {
            return Err(ConfigError::new(
                E_MESSENGER_MISSING,
                format!(
                    "messenger is '{}' but the {section} section is missing",
                    messenger.as_str()
                ),
            ));
        }
        let omp_path = file
            .omp_path
            .ok_or_else(|| ConfigError::new(E_OMP_MISSING, "omp_path is required"))?;

        let omp = file.omp.unwrap_or(OmpSection {
            turn_timeout_minutes: None,
            max_cost_usd: None,
        });
        if omp.turn_timeout_minutes == Some(0) {
            return Err(ConfigError::new(
                E_CONFIG_VALUE,
                "omp.turn_timeout_minutes must be greater than zero (omit it for no limit)",
            ));
        }
        if let Some(cost) = omp.max_cost_usd
            && !(cost.is_finite() && cost > 0.0)
        {
            return Err(ConfigError::new(
                E_CONFIG_VALUE,
                "omp.max_cost_usd must be greater than zero (omit it for no limit)",
            ));
        }

        let home = match file.home {
            Some(home) => env.expand_tilde(&home)?,
            None => env.default_home()?,
        };
        let work_dir =
            env.expand_tilde(&file.work_dir.unwrap_or_else(|| DEFAULT_WORK_DIR.into()))?;

        Ok(Config {
            messenger,
            discord,
            slack,
            work_dir,
            omp_path: env.expand_tilde(&omp_path)?,
            home,
            access_mode,
            omp: OmpConfig {
                turn_timeout_minutes: omp.turn_timeout_minutes,
                max_cost_usd: omp.max_cost_usd,
            },
        })
    }

    /// Serializes to `config.toml` text, including the resolved `home`.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        let has_omp_limits =
            self.omp.turn_timeout_minutes.is_some() || self.omp.max_cost_usd.is_some();
        let file = ConfigFile {
            messenger: Some(self.messenger.as_str().to_string()),
            owner_id: None,
            token_file: None,
            work_dir: Some(self.work_dir.clone()),
            omp_path: Some(self.omp_path.clone()),
            home: Some(self.home.clone()),
            access_mode: Some(self.access_mode.as_str().to_string()),
            discord: self.discord.as_ref().map(|d| DiscordSection {
                owner_id: Some(d.owner_id),
                channel_id: Some(d.channel_id),
                token_file: Some(d.token_file.clone()),
            }),
            slack: self.slack.as_ref().map(|s| SlackSection {
                owner_id: Some(s.owner_id.clone()),
                channel_id: Some(s.channel_id.clone()),
                bot_token_file: Some(s.bot_token_file.clone()),
                app_token_file: Some(s.app_token_file.clone()),
            }),
            omp: has_omp_limits.then_some(OmpSection {
                turn_timeout_minutes: self.omp.turn_timeout_minutes,
                max_cost_usd: self.omp.max_cost_usd,
            }),
        };
        toml::to_string(&file).map_err(|e| ConfigError::new(E_CONFIG_VALUE, e.to_string()))
    }

    /// The active messenger's owner id as text.
    pub fn owner_id(&self) -> String {
        match self.messenger {
            Platform::Discord => self.discord.as_ref().map(|d| d.owner_id.to_string()),
            Platform::Slack => self.slack.as_ref().map(|s| s.owner_id.clone()),
        }
        .unwrap_or_default()
    }

    /// The active messenger's channel id as text.
    pub fn channel_id(&self) -> String {
        match self.messenger {
            Platform::Discord => self.discord.as_ref().map(|d| d.channel_id.to_string()),
            Platform::Slack => self.slack.as_ref().map(|s| s.channel_id.clone()),
        }
        .unwrap_or_default()
    }

    /// Runs every check, creating the work folder and the home layout when missing.
    /// Token files are checked for every configured messenger; the inactive one is advisory.
    pub fn checks(&self) -> Vec<Check> {
        let active = self.messenger;
        let mut checks = vec![
            Check {
                name: "owner_id",
                result: self.check_owner(),
                advisory: false,
            },
            Check {
                name: match active {
                    Platform::Discord => "discord.channel_id",
                    Platform::Slack => "slack.channel_id",
                },
                result: self.check_channel(),
                advisory: false,
            },
        ];
        if let Some(discord) = &self.discord {
            checks.push(Check {
                name: "discord token_file",
                result: check_token_file(&discord.token_file),
                advisory: active != Platform::Discord,
            });
        }
        if let Some(slack) = &self.slack {
            let advisory = active != Platform::Slack;
            checks.push(Check {
                name: "slack bot_token_file",
                result: check_token_file(&slack.bot_token_file),
                advisory,
            });
            checks.push(Check {
                name: "slack app_token_file",
                result: check_token_file(&slack.app_token_file),
                advisory,
            });
        }
        checks.extend([
            Check {
                name: "work_dir",
                result: self.check_work_dir(),
                advisory: false,
            },
            Check {
                name: "omp_path",
                result: self.check_omp(),
                advisory: false,
            },
            Check {
                name: "home layout",
                result: self.ensure_home_layout(),
                advisory: false,
            },
            Check {
                name: "hints",
                result: self.check_hints(),
                advisory: false,
            },
        ]);
        checks
    }

    /// Returns the first failing check of the active messenger and the shared settings.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.checks()
            .into_iter()
            .filter(|check| !check.advisory)
            .try_for_each(|check| check.result.map(drop))
    }

    /// Creates the home folder with `conversations/`, `logs/` and `secrets/`, all mode 0700.
    pub fn ensure_home_layout(&self) -> Result<String, ConfigError> {
        for dir in [
            self.home.clone(),
            self.home.join("conversations"),
            self.home.join("logs"),
            self.home.join("secrets"),
        ] {
            ensure_private_dir(&dir).map_err(|e| {
                ConfigError::io(
                    E_HOME_LAYOUT,
                    &format!("cannot prepare {}", dir.display()),
                    &e,
                )
            })?;
        }
        Ok(self.home.display().to_string())
    }

    fn check_owner(&self) -> Result<String, ConfigError> {
        if self.owner_id().trim_matches('0').is_empty() {
            return Err(ConfigError::new(
                E_OWNER_MISSING,
                "owner_id must be a non-zero user id",
            ));
        }
        Ok("set".to_string())
    }

    fn check_channel(&self) -> Result<String, ConfigError> {
        let channel = match self.messenger {
            Platform::Discord => self.discord.as_ref().map(|d| d.channel_id.to_string()),
            Platform::Slack => self.slack.as_ref().map(|s| s.channel_id.clone()),
        }
        .unwrap_or_default();
        if channel.trim_matches('0').is_empty() {
            return Err(ConfigError::new(
                E_CHANNEL_MISSING,
                format!(
                    "{}.channel_id must be a non-zero channel id",
                    self.messenger.as_str()
                ),
            ));
        }
        Ok("set".to_string())
    }

    fn check_work_dir(&self) -> Result<String, ConfigError> {
        let path = &self.work_dir;
        if path.is_dir() {
            return Ok(format!("{} (exists)", path.display()));
        }
        if path.exists() {
            return Err(ConfigError::new(
                E_WORK_DIR,
                format!("{} exists but is not a folder", path.display()),
            ));
        }
        fs::create_dir_all(path).map_err(|e| {
            ConfigError::io(E_WORK_DIR, &format!("cannot create {}", path.display()), &e)
        })?;
        Ok(format!("{} (created)", path.display()))
    }

    /// Validates only the active messenger's override file; the built-in defaults live in
    /// the adapters.
    fn check_hints(&self) -> Result<String, ConfigError> {
        let platform = self.messenger.as_str();
        let path = crate::hints::override_path(&self.home, platform);
        if !path.exists() {
            return Ok("default when absent".to_owned());
        }
        crate::hints::load(&self.home, platform, "")?;
        Ok(format!("override {}", path.display()))
    }

    fn check_omp(&self) -> Result<String, ConfigError> {
        let path = &self.omp_path;
        let meta = fs::metadata(path).map_err(|e| {
            ConfigError::io(
                E_OMP_MISSING,
                &format!("cannot find omp at {}", path.display()),
                &e,
            )
        })?;
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return Err(ConfigError::new(
                E_OMP_MISSING,
                format!("{} is not an executable file", path.display()),
            ));
        }
        Ok(path.display().to_string())
    }
}

fn check_token_file(path: &Path) -> Result<String, ConfigError> {
    let meta = fs::metadata(path).map_err(|e| {
        ConfigError::io(
            E_TOKEN_MISSING,
            &format!("cannot read token file {}", path.display()),
            &e,
        )
    })?;
    if !meta.is_file() {
        return Err(ConfigError::new(
            E_TOKEN_NOT_FILE,
            format!("token file {} is not a regular file", path.display()),
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(ConfigError::new(
            E_TOKEN_MODE,
            format!(
                "token file {} has mode {mode:03o}; only the owner may access it, run: chmod 600 {}",
                path.display(),
                path.display()
            ),
        ));
    }
    Ok(format!("{} (mode {mode:03o})", path.display()))
}

/// Merges the `[discord]` section with the slice-1 top-level keys. A key given in both
/// places is ambiguous. `None` means no Discord settings at all.
fn discord_config(
    section: Option<DiscordSection>,
    legacy_owner: Option<u64>,
    legacy_token: Option<PathBuf>,
    env: &Env,
) -> Result<Option<DiscordConfig>, ConfigError> {
    if section.is_none() && legacy_owner.is_none() && legacy_token.is_none() {
        return Ok(None);
    }
    let section = section.unwrap_or_default();
    if legacy_owner.is_some() && section.owner_id.is_some() {
        return Err(ConfigError::new(
            E_CONFIG_VALUE,
            "owner_id is set both at the top level and in [discord]; keep only the [discord] one",
        ));
    }
    if legacy_token.is_some() && section.token_file.is_some() {
        return Err(ConfigError::new(
            E_CONFIG_VALUE,
            "token_file is set both at the top level and in [discord]; keep only the [discord] one",
        ));
    }
    let owner_id = legacy_owner
        .or(section.owner_id)
        .ok_or_else(|| ConfigError::new(E_OWNER_MISSING, "owner_id is required"))?;
    let channel_id = section
        .channel_id
        .ok_or_else(|| ConfigError::new(E_CHANNEL_MISSING, "discord.channel_id is required"))?;
    let token_file = legacy_token
        .or(section.token_file)
        .ok_or_else(|| ConfigError::new(E_TOKEN_MISSING, "token_file is required"))?;
    Ok(Some(DiscordConfig {
        owner_id,
        channel_id,
        token_file: env.expand_tilde(&token_file)?,
    }))
}

fn slack_config(
    section: Option<SlackSection>,
    env: &Env,
) -> Result<Option<SlackConfig>, ConfigError> {
    let Some(section) = section else {
        return Ok(None);
    };
    let owner_id = section
        .owner_id
        .ok_or_else(|| ConfigError::new(E_OWNER_MISSING, "slack.owner_id is required"))?;
    let channel_id = section
        .channel_id
        .ok_or_else(|| ConfigError::new(E_CHANNEL_MISSING, "slack.channel_id is required"))?;
    let bot_token_file = section
        .bot_token_file
        .ok_or_else(|| ConfigError::new(E_TOKEN_MISSING, "slack.bot_token_file is required"))?;
    let app_token_file = section
        .app_token_file
        .ok_or_else(|| ConfigError::new(E_TOKEN_MISSING, "slack.app_token_file is required"))?;
    check_slack_id("slack.owner_id", &owner_id, &['U', 'W'])?;
    check_slack_id("slack.channel_id", &channel_id, &['C', 'G'])?;
    Ok(Some(SlackConfig {
        owner_id,
        channel_id,
        bot_token_file: env.expand_tilde(&bot_token_file)?,
        app_token_file: env.expand_tilde(&app_token_file)?,
    }))
}

/// A Slack id is an uppercase letter or digit string with a kind prefix.
fn check_slack_id(name: &str, id: &str, prefixes: &[char]) -> Result<(), ConfigError> {
    let mut chars = id.chars();
    let well_formed = chars.next().is_some_and(|c| prefixes.contains(&c))
        && id.len() > 1
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if well_formed {
        return Ok(());
    }
    Err(ConfigError::new(
        E_CONFIG_VALUE,
        format!(
            "{name} must start with {} followed by uppercase letters or digits",
            prefixes.iter().collect::<String>()
        ),
    ))
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Prints each check as `OK`/`FAIL`. Returns true when every check passed.
pub fn report_checks(checks: &[Check], out: &mut impl io::Write) -> io::Result<bool> {
    let mut all_ok = true;
    for check in checks {
        match &check.result {
            Ok(detail) => writeln!(out, "OK   {}: {detail}", check.name)?,
            Err(err) if check.advisory => writeln!(out, "WARN {}: {err}", check.name)?,
            Err(err) => {
                all_ok = false;
                writeln!(out, "FAIL {}: {err}", check.name)?;
            }
        }
    }
    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn quoted(path: &Path) -> String {
        toml::Value::from(path.to_str().expect("utf-8 path")).to_string()
    }

    struct Fixture {
        dir: TempDir,
        token: PathBuf,
        omp: PathBuf,
        env: Env,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let token = dir.path().join("token");
            fs::write(&token, "TOKEN_SENTINEL_9f3a").expect("write token");
            fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("chmod token");
            let omp = dir.path().join("omp");
            fs::write(&omp, "#!/bin/sh\n").expect("write omp");
            fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).expect("chmod omp");
            let env = Env {
                home_dir: Some(dir.path().to_path_buf()),
                moraebot_home: None,
                path: None,
            };
            Self {
                dir,
                token,
                omp,
                env,
            }
        }

        fn valid_toml(&self) -> String {
            format!(
                "owner_id = 111\ntoken_file = {}\nomp_path = {}\n[discord]\nchannel_id = 222\n",
                quoted(&self.token),
                quoted(&self.omp)
            )
        }

        fn load(&self, text: &str) -> Result<Config, ConfigError> {
            Config::from_toml(text, &self.env)
        }
    }

    fn code<T: fmt::Debug>(result: Result<T, ConfigError>) -> &'static str {
        result.expect_err("expected an error").code
    }

    #[test]
    fn missing_owner_id_is_an_error() {
        let fx = Fixture::new();
        let text = fx.valid_toml().replace("owner_id = 111\n", "");
        assert_eq!(code(fx.load(&text)), E_OWNER_MISSING);
    }

    #[test]
    fn channel_id_follows_the_active_messenger() {
        let fx = Fixture::new();
        let mut cfg = fx.load(&fx.valid_toml()).expect("loads");
        assert_eq!(cfg.channel_id(), "222");
        cfg.messenger = Platform::Slack;
        assert_eq!(cfg.channel_id(), "");
        cfg.slack = Some(SlackConfig {
            owner_id: "U111".to_string(),
            channel_id: "C222".to_string(),
            bot_token_file: fx.token.clone(),
            app_token_file: fx.token.clone(),
        });
        assert_eq!(cfg.channel_id(), "C222");
    }

    #[test]
    fn owner_id_zero_fails_validation() {
        let fx = Fixture::new();
        let text = fx.valid_toml().replace("owner_id = 111", "owner_id = 0");
        let cfg = fx.load(&text).expect("loads");
        assert_eq!(code(cfg.validate()), E_OWNER_MISSING);
    }

    #[test]
    fn missing_channel_id_is_an_error() {
        let fx = Fixture::new();
        let text = fx.valid_toml().replace("[discord]\nchannel_id = 222\n", "");
        assert_eq!(code(fx.load(&text)), E_CHANNEL_MISSING);
    }

    #[test]
    fn channel_id_zero_fails_validation() {
        let fx = Fixture::new();
        let text = fx
            .valid_toml()
            .replace("channel_id = 222", "channel_id = 0");
        let cfg = fx.load(&text).expect("loads");
        assert_eq!(code(cfg.validate()), E_CHANNEL_MISSING);
    }

    #[test]
    fn token_file_mode_0644_is_rejected() {
        let fx = Fixture::new();
        fs::set_permissions(&fx.token, fs::Permissions::from_mode(0o644)).expect("chmod");
        let cfg = fx.load(&fx.valid_toml()).expect("loads");
        assert_eq!(code(cfg.validate()), E_TOKEN_MODE);
    }

    #[test]
    fn missing_token_file_is_rejected() {
        let fx = Fixture::new();
        fs::remove_file(&fx.token).expect("remove");
        let cfg = fx.load(&fx.valid_toml()).expect("loads");
        assert_eq!(code(cfg.validate()), E_TOKEN_MISSING);
    }

    #[test]
    fn token_path_that_is_a_directory_is_rejected() {
        let fx = Fixture::new();
        let text = fx
            .valid_toml()
            .replace(&quoted(&fx.token), &quoted(fx.dir.path()));
        let cfg = fx.load(&text).expect("loads");
        assert_eq!(code(cfg.validate()), E_TOKEN_NOT_FILE);
    }

    #[test]
    fn unknown_key_is_rejected() {
        let fx = Fixture::new();
        let text = format!("{}surprise = 1\n", fx.valid_toml());
        assert_eq!(code(fx.load(&text)), E_CONFIG_PARSE);
    }

    #[test]
    fn unknown_nested_key_is_rejected() {
        let fx = Fixture::new();
        let text = format!("{}surprise = 1\n", fx.valid_toml());
        assert_eq!(code(fx.load(&text)), E_CONFIG_PARSE);
        let nested = format!("{}[omp]\nsurprise = 1\n", fx.valid_toml());
        assert_eq!(code(fx.load(&nested)), E_CONFIG_PARSE);
    }

    #[test]
    fn access_mode_open_is_a_stage_2_error() {
        let fx = Fixture::new();
        let text = format!("access_mode = \"open\"\n{}", fx.valid_toml());
        assert_eq!(code(fx.load(&text)), E_ACCESS_MODE);
    }

    #[test]
    fn owner_only_access_mode_is_accepted() {
        let fx = Fixture::new();
        let text = format!("access_mode = \"owner_only\"\n{}", fx.valid_toml());
        assert_eq!(
            fx.load(&text).expect("loads").access_mode,
            AccessMode::OwnerOnly
        );
    }

    #[test]
    fn omp_path_is_required() {
        let fx = Fixture::new();
        let text = fx
            .valid_toml()
            .replace(&format!("omp_path = {}\n", quoted(&fx.omp)), "");
        assert_eq!(code(fx.load(&text)), E_OMP_MISSING);
    }

    #[test]
    fn omp_path_must_exist_and_be_executable() {
        let fx = Fixture::new();
        fs::set_permissions(&fx.omp, fs::Permissions::from_mode(0o644)).expect("chmod");
        let cfg = fx.load(&fx.valid_toml()).expect("loads");
        assert_eq!(code(cfg.validate()), E_OMP_MISSING);
        fs::remove_file(&fx.omp).expect("remove");
        assert_eq!(code(cfg.validate()), E_OMP_MISSING);
    }

    #[test]
    fn valid_config_creates_work_dir_and_private_home_layout() {
        let fx = Fixture::new();
        let home = fx.dir.path().join("home");
        let work = fx.dir.path().join("work");
        let env = Env {
            moraebot_home: Some(home.clone()),
            ..fx.env.clone()
        };
        let text = format!("work_dir = {}\n{}", quoted(&work), fx.valid_toml());
        let cfg = Config::from_toml(&text, &env).expect("loads");
        cfg.validate().expect("valid");
        assert!(work.is_dir());
        for dir in [
            &home,
            &home.join("conversations"),
            &home.join("logs"),
            &home.join("secrets"),
        ] {
            let mode = fs::metadata(dir).expect("dir exists").permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
    }

    #[test]
    fn defaults_come_from_the_environment() {
        let fx = Fixture::new();
        let cfg = fx.load(&fx.valid_toml()).expect("loads");
        assert_eq!(cfg.work_dir, fx.dir.path().join("moraebot-work"));
        assert_eq!(cfg.home, fx.dir.path().join(".moraebot"));
        assert_eq!(cfg.omp.turn_timeout_minutes, None);
        assert_eq!(cfg.omp.max_cost_usd, None);
    }

    #[test]
    fn config_home_wins_over_env_and_env_is_the_fallback() {
        let fx = Fixture::new();
        let env_home = fx.dir.path().join("from-env");
        let file_home = fx.dir.path().join("from-file");
        let env = Env {
            moraebot_home: Some(env_home.clone()),
            ..fx.env.clone()
        };
        let with_home = format!("home = {}\n{}", quoted(&file_home), fx.valid_toml());
        assert_eq!(
            Config::from_toml(&with_home, &env).expect("loads").home,
            file_home
        );
        assert_eq!(
            Config::from_toml(&fx.valid_toml(), &env)
                .expect("loads")
                .home,
            env_home
        );
    }

    #[test]
    fn optional_limits_are_parsed() {
        let fx = Fixture::new();
        let text = format!(
            "{}[omp]\nturn_timeout_minutes = 30\nmax_cost_usd = 2.5\n",
            fx.valid_toml()
        );
        let cfg = fx.load(&text).expect("loads");
        assert_eq!(cfg.omp.turn_timeout_minutes, Some(30));
        assert_eq!(cfg.omp.max_cost_usd, Some(2.5));
    }

    #[test]
    fn zero_limits_are_rejected() {
        let fx = Fixture::new();
        let timeout = format!("{}[omp]\nturn_timeout_minutes = 0\n", fx.valid_toml());
        assert_eq!(code(fx.load(&timeout)), E_CONFIG_VALUE);
        let cost = format!("{}[omp]\nmax_cost_usd = 0.0\n", fx.valid_toml());
        assert_eq!(code(fx.load(&cost)), E_CONFIG_VALUE);
    }

    #[test]
    fn debug_shows_token_path_but_never_token_content() {
        let fx = Fixture::new();
        let cfg = fx.load(&fx.valid_toml()).expect("loads");
        let shown = format!("{cfg:?}");
        assert!(shown.contains(fx.token.to_str().expect("utf-8 path")));
        assert!(!shown.contains("TOKEN_SENTINEL_9f3a"));
    }

    #[test]
    fn to_toml_round_trips() {
        let fx = Fixture::new();
        let text = format!("{}[omp]\nturn_timeout_minutes = 5\n", fx.valid_toml());
        let cfg = fx.load(&text).expect("loads");
        let again = fx
            .load(&cfg.to_toml().expect("serializes"))
            .expect("reloads");
        assert_eq!(again.owner_id(), cfg.owner_id());
        assert_eq!(again.home, cfg.home);
        assert_eq!(again.work_dir, cfg.work_dir);
        assert_eq!(again.omp_path, cfg.omp_path);
        assert_eq!(again.omp.turn_timeout_minutes, Some(5));
    }

    fn slack_toml(fx: &Fixture) -> String {
        format!(
            "messenger = \"slack\"\nomp_path = {}\n[slack]\nowner_id = \"U111\"\nchannel_id = \"C222\"\n\
             bot_token_file = {}\napp_token_file = {}\n",
            quoted(&fx.omp),
            quoted(&fx.token),
            quoted(&fx.token)
        )
    }

    #[test]
    fn legacy_discord_file_loads() {
        let fx = Fixture::new();
        let cfg = fx.load(&fx.valid_toml()).expect("loads");
        assert_eq!(cfg.messenger, Platform::Discord);
        let discord = cfg.discord.expect("discord section");
        assert_eq!(discord.owner_id, 111);
        assert_eq!(discord.channel_id, 222);
        assert_eq!(discord.token_file, fx.token);
        assert!(cfg.slack.is_none());
    }

    #[test]
    fn legacy_and_section_owner_conflict_is_config_value() {
        let fx = Fixture::new();
        let owner = fx
            .valid_toml()
            .replace("[discord]\n", "[discord]\nowner_id = 333\n");
        assert_eq!(code(fx.load(&owner)), E_CONFIG_VALUE);
        let token = fx.valid_toml().replace(
            "[discord]\n",
            &format!("[discord]\ntoken_file = {}\n", quoted(&fx.token)),
        );
        assert_eq!(code(fx.load(&token)), E_CONFIG_VALUE);
    }

    #[test]
    fn slack_section_loads() {
        let fx = Fixture::new();
        let cfg = fx.load(&slack_toml(&fx)).expect("loads");
        assert_eq!(cfg.messenger, Platform::Slack);
        assert_eq!(cfg.owner_id(), "U111");
        let slack = cfg.slack.expect("slack section");
        assert_eq!(slack.channel_id, "C222");
        assert_eq!(slack.bot_token_file, fx.token);
        assert_eq!(slack.app_token_file, fx.token);
        assert!(cfg.discord.is_none());
    }

    #[test]
    fn slack_active_without_section_is_messenger_missing() {
        let fx = Fixture::new();
        let text = format!("messenger = \"slack\"\nomp_path = {}\n", quoted(&fx.omp));
        assert_eq!(code(fx.load(&text)), E_MESSENGER_MISSING);
        let discord_only = format!("messenger = \"slack\"\n{}", fx.valid_toml());
        assert_eq!(code(fx.load(&discord_only)), E_MESSENGER_MISSING);
    }

    #[test]
    fn unknown_messenger_rejected() {
        let fx = Fixture::new();
        let text = format!("messenger = \"telegram\"\n{}", fx.valid_toml());
        assert_eq!(code(fx.load(&text)), E_MESSENGER);
    }

    #[test]
    fn slack_owner_id_format_rejected() {
        let fx = Fixture::new();
        for owner in ["111", "u111", "U", "UAB-1", "C111"] {
            let text = slack_toml(&fx).replace("U111", owner);
            assert_eq!(code(fx.load(&text)), E_CONFIG_VALUE, "owner {owner:?}");
        }
        for channel in ["222", "D222", "c222"] {
            let text = slack_toml(&fx).replace("C222", channel);
            assert_eq!(code(fx.load(&text)), E_CONFIG_VALUE, "channel {channel:?}");
        }
        let workspace_user = slack_toml(&fx)
            .replace("U111", "W111")
            .replace("C222", "G222");
        assert!(fx.load(&workspace_user).is_ok());
    }

    #[test]
    fn to_toml_writes_new_layout_and_round_trips() {
        let fx = Fixture::new();
        let cfg = fx.load(&fx.valid_toml()).expect("loads");
        let text = cfg.to_toml().expect("serializes");
        let top_level = text.split('[').next().expect("text before first table");
        assert!(top_level.contains("messenger = \"discord\""));
        assert!(!top_level.contains("owner_id"));
        assert!(!top_level.contains("token_file"));
        let again = fx.load(&text).expect("reloads");
        let (a, b) = (
            again.discord.expect("discord"),
            cfg.discord.expect("discord"),
        );
        assert_eq!(
            (a.owner_id, a.channel_id, a.token_file),
            (b.owner_id, b.channel_id, b.token_file)
        );

        let slack = fx.load(&slack_toml(&fx)).expect("loads");
        let again = fx
            .load(&slack.to_toml().expect("serializes"))
            .expect("reloads");
        assert_eq!(again.messenger, Platform::Slack);
        assert_eq!(again.owner_id(), "U111");
    }

    #[test]
    fn checks_cover_both_messengers() {
        let fx = Fixture::new();
        let other = fx.dir.path().join("other.token");
        fs::write(&other, "x").expect("write");
        fs::set_permissions(&other, fs::Permissions::from_mode(0o644)).expect("chmod");
        let both = format!(
            "{}[slack]\nowner_id = \"U111\"\nchannel_id = \"C222\"\nbot_token_file = {}\napp_token_file = {}\n",
            fx.valid_toml(),
            quoted(&other),
            quoted(&fx.token)
        );
        let cfg = fx.load(&both).expect("loads");
        let names: Vec<_> = cfg.checks().iter().map(|c| c.name).collect();
        for name in [
            "discord token_file",
            "slack bot_token_file",
            "slack app_token_file",
        ] {
            assert!(names.contains(&name), "{name}");
        }
        // The inactive messenger's bad token file is advisory, so validation still passes.
        cfg.validate().expect("active messenger is fine");
        let mut out = Vec::new();
        let all_ok = report_checks(&cfg.checks(), &mut out).expect("report");
        assert!(all_ok);
        let report = String::from_utf8(out).expect("utf-8");
        assert!(report.contains("WARN slack bot_token_file"));
        assert!(report.contains(E_TOKEN_MODE));

        // Make Slack the active messenger: the same bad file now fails validation.
        let slack_active = format!("messenger = \"slack\"\n{both}");
        let cfg = fx.load(&slack_active).expect("loads");
        assert_eq!(code(cfg.validate()), E_TOKEN_MODE);
    }

    #[test]
    fn tilde_needs_home() {
        let env = Env::default();
        assert_eq!(code(env.expand_tilde(Path::new("~/x"))), E_HOME_UNKNOWN);
        assert_eq!(
            env.expand_tilde(Path::new("/abs")).expect("ok"),
            PathBuf::from("/abs")
        );
    }
}
