//! `slack-bridge.toml`: the Slack bridge's own config.
//!
//! It names the socket the bridge listens on, the Slack channel it reports as `home`
//! and the two token files. Access decisions (owner, allow list) stay in the core's
//! `config.toml`; the bridge receives them in `hello_ok`. Like `config.toml`, this
//! file only points at the token files and the tokens are never read into
//! [`SlackBridgeConfig`].

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bridge::socket;
use crate::config::{
    Check, ConfigError, E_CHANNEL_MISSING, E_CONFIG_PARSE, E_CONFIG_READ, E_CONFIG_VALUE,
    E_TOKEN_MISSING, E_TOKEN_MODE, E_TOKEN_NOT_FILE, Env,
};

/// File name of the bridge config inside the moraebot home.
pub const FILE_NAME: &str = "slack-bridge.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackBridgeConfig {
    /// Absolute socket path, the same one the core's `[bridge] socket` names.
    pub socket: PathBuf,
    /// The channel the bridge reports as `home` (`C...` or `G...`).
    pub channel_id: String,
    pub bot_token_file: PathBuf,
    pub app_token_file: PathBuf,
}

/// On-disk shape of `slack-bridge.toml`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SlackBridgeFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    socket: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bot_token_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_token_file: Option<PathBuf>,
}

/// `<moraebot home>/slack-bridge.toml`.
pub fn default_path(env: &Env) -> Result<PathBuf, ConfigError> {
    Ok(env.default_home()?.join(FILE_NAME))
}

impl SlackBridgeConfig {
    pub fn load(path: &Path, env: &Env) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|e| {
            ConfigError::io(
                E_CONFIG_READ,
                &format!("cannot read {}", path.display()),
                &e,
            )
        })?;
        Self::from_toml(&text, env)
    }

    /// Parses config text: required keys, the channel id format and the socket path
    /// shape. Filesystem checks live in [`SlackBridgeConfig::validate`].
    pub fn from_toml(text: &str, env: &Env) -> Result<Self, ConfigError> {
        let file: SlackBridgeFile =
            toml::from_str(text).map_err(|e| ConfigError::new(E_CONFIG_PARSE, e.to_string()))?;
        let socket = file
            .socket
            .ok_or_else(|| ConfigError::new(E_CONFIG_VALUE, "socket is required"))?;
        let channel_id = file
            .channel_id
            .ok_or_else(|| ConfigError::new(E_CHANNEL_MISSING, "channel_id is required"))?;
        let bot_token_file = file
            .bot_token_file
            .ok_or_else(|| ConfigError::new(E_TOKEN_MISSING, "bot_token_file is required"))?;
        let app_token_file = file
            .app_token_file
            .ok_or_else(|| ConfigError::new(E_TOKEN_MISSING, "app_token_file is required"))?;
        check_slack_id("channel_id", &channel_id, &['C', 'G'])?;
        let socket = env.expand_tilde(&socket)?;
        socket::check_path(&socket)?;
        Ok(Self {
            socket,
            channel_id,
            bot_token_file: env.expand_tilde(&bot_token_file)?,
            app_token_file: env.expand_tilde(&app_token_file)?,
        })
    }

    /// Serializes to `slack-bridge.toml` text with the resolved paths.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        let file = SlackBridgeFile {
            socket: Some(self.socket.clone()),
            channel_id: Some(self.channel_id.clone()),
            bot_token_file: Some(self.bot_token_file.clone()),
            app_token_file: Some(self.app_token_file.clone()),
        };
        toml::to_string(&file).map_err(|e| ConfigError::new(E_CONFIG_VALUE, e.to_string()))
    }

    /// The `check-config` lines: both token files exist, are regular files and are
    /// private to the owner.
    pub fn checks(&self) -> Vec<Check> {
        vec![
            Check {
                name: "bot_token_file",
                result: check_token_file(&self.bot_token_file),
                advisory: false,
            },
            Check {
                name: "app_token_file",
                result: check_token_file(&self.app_token_file),
                advisory: false,
            },
        ]
    }

    /// The first failing check, if any.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.checks()
            .into_iter()
            .filter(|check| !check.advisory)
            .try_for_each(|check| check.result.map(drop))
    }
}

/// A Slack id is an uppercase letter or digit string with a kind prefix.
pub fn check_slack_id(name: &str, id: &str, prefixes: &[char]) -> Result<(), ConfigError> {
    let well_formed = id.chars().next().is_some_and(|c| prefixes.contains(&c))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::E_BRIDGE_SOCKET;

    struct Fixture {
        tmp: tempfile::TempDir,
        bot: PathBuf,
        app: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let bot = tmp.path().join("slack-bot.token");
        let app = tmp.path().join("slack-app.token");
        for path in [&bot, &app] {
            fs::write(path, "x").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        Fixture { tmp, bot, app }
    }

    fn env() -> Env {
        Env {
            home_dir: Some(PathBuf::from("/home/u")),
            moraebot_home: None,
            path: None,
        }
    }

    fn text(fx: &Fixture) -> String {
        format!(
            "socket = \"/run/user/1000/moraebot/slack-bridge.sock\"\n\
             channel_id = \"C222BBBB\"\n\
             bot_token_file = {:?}\n\
             app_token_file = {:?}\n",
            fx.bot, fx.app
        )
    }

    fn code(result: Result<SlackBridgeConfig, ConfigError>) -> &'static str {
        result.expect_err("should fail").code
    }

    #[test]
    fn a_full_file_loads_from_disk() {
        let fx = fixture();
        let path = fx.tmp.path().join(FILE_NAME);
        fs::write(&path, text(&fx)).unwrap();
        let config = SlackBridgeConfig::load(&path, &env()).unwrap();
        assert_eq!(
            config,
            SlackBridgeConfig {
                socket: PathBuf::from("/run/user/1000/moraebot/slack-bridge.sock"),
                channel_id: "C222BBBB".into(),
                bot_token_file: fx.bot.clone(),
                app_token_file: fx.app.clone(),
            }
        );
        config.validate().expect("token files are private");
    }

    #[test]
    fn a_missing_file_is_a_read_error() {
        let fx = fixture();
        let err = SlackBridgeConfig::load(&fx.tmp.path().join("nope.toml"), &env()).unwrap_err();
        assert_eq!(err.code, E_CONFIG_READ);
    }

    #[test]
    fn to_toml_round_trips() {
        let fx = fixture();
        let config = SlackBridgeConfig::from_toml(&text(&fx), &env()).unwrap();
        let again = SlackBridgeConfig::from_toml(&config.to_toml().unwrap(), &env()).unwrap();
        assert_eq!(again, config);
    }

    #[test]
    fn tilde_paths_are_expanded() {
        let input = "socket = \"~/.moraebot/run/slack-bridge.sock\"\n\
                     channel_id = \"G1\"\n\
                     bot_token_file = \"~/.moraebot/secrets/slack-bot.token\"\n\
                     app_token_file = \"~/.moraebot/secrets/slack-app.token\"\n";
        let config = SlackBridgeConfig::from_toml(input, &env()).unwrap();
        assert_eq!(
            config.socket,
            PathBuf::from("/home/u/.moraebot/run/slack-bridge.sock")
        );
        assert_eq!(
            config.bot_token_file,
            PathBuf::from("/home/u/.moraebot/secrets/slack-bot.token")
        );
        assert_eq!(
            config.app_token_file,
            PathBuf::from("/home/u/.moraebot/secrets/slack-app.token")
        );
    }

    #[test]
    fn each_missing_key_has_its_code() {
        let fx = fixture();
        let full = text(&fx);
        for (key, expected) in [
            ("socket", E_CONFIG_VALUE),
            ("channel_id", E_CHANNEL_MISSING),
            ("bot_token_file", E_TOKEN_MISSING),
            ("app_token_file", E_TOKEN_MISSING),
        ] {
            let without: String = full
                .lines()
                .filter(|line| !line.starts_with(&format!("{key} =")))
                .map(|line| format!("{line}\n"))
                .collect();
            assert_eq!(
                code(SlackBridgeConfig::from_toml(&without, &env())),
                expected,
                "{key}"
            );
        }
    }

    #[test]
    fn unknown_keys_and_core_keys_are_refused() {
        let fx = fixture();
        for extra in ["owner_id = \"U111AAAA\"\n", "[[allow]]\nid = \"U1\"\n"] {
            let input = format!("{}{extra}", text(&fx));
            assert_eq!(
                code(SlackBridgeConfig::from_toml(&input, &env())),
                E_CONFIG_PARSE,
                "{extra}"
            );
        }
    }

    #[test]
    fn a_bad_channel_id_is_refused() {
        let fx = fixture();
        for bad in ["D333", "U111AAAA", "C", "c222", "C22 2", ""] {
            let input = text(&fx).replace("C222BBBB", bad);
            assert_eq!(
                code(SlackBridgeConfig::from_toml(&input, &env())),
                E_CONFIG_VALUE,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_relative_or_too_long_socket_is_refused() {
        let fx = fixture();
        let long = format!("/{}", "a".repeat(socket::SUN_PATH_LEN));
        for bad in ["run/slack-bridge.sock", long.as_str()] {
            let input = text(&fx).replace("/run/user/1000/moraebot/slack-bridge.sock", bad);
            assert_eq!(
                code(SlackBridgeConfig::from_toml(&input, &env())),
                E_BRIDGE_SOCKET,
                "{bad}"
            );
        }
    }

    #[test]
    fn token_files_must_exist_and_be_private() {
        let fx = fixture();
        let config = SlackBridgeConfig::from_toml(&text(&fx), &env()).unwrap();

        fs::set_permissions(&fx.app, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(config.validate().unwrap_err().code, E_TOKEN_MODE);

        fs::remove_file(&fx.app).unwrap();
        fs::create_dir(&fx.app).unwrap();
        assert_eq!(config.validate().unwrap_err().code, E_TOKEN_NOT_FILE);

        fs::remove_file(&fx.bot).unwrap();
        assert_eq!(config.validate().unwrap_err().code, E_TOKEN_MISSING);
    }

    #[test]
    fn the_default_path_is_in_the_moraebot_home() {
        assert_eq!(
            default_path(&env()).unwrap(),
            PathBuf::from("/home/u/.moraebot/slack-bridge.toml")
        );
        let custom = Env {
            moraebot_home: Some(PathBuf::from("/srv/mb")),
            ..env()
        };
        assert_eq!(
            default_path(&custom).unwrap(),
            PathBuf::from("/srv/mb/slack-bridge.toml")
        );
    }
}
