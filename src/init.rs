//! Interactive `moraebot init`.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::config::{
    AccessMode, Config, ConfigError, DiscordConfig, E_CHANNEL_MISSING, E_IO, E_OMP_MISSING,
    E_OWNER_MISSING, Env, OmpConfig, Platform, SlackConfig, report_checks,
};
use crate::onboarding::{E_ONBOARDING_REQUEST, request};

pub const E_INIT_EOF: &str = "E_INIT_EOF";
pub const E_SETUP_NO_CONFIG: &str = "E_SETUP_NO_CONFIG";
pub const E_SLACK_TOKEN_KIND: &str = "E_SLACK_TOKEN_KIND";

/// Printed after `moraebot init` when the new config passed every check.
pub const NEXT_STEPS: &str = "\
Next steps:
  moraebot check-config     check the setup again at any time
  moraebot install-launchd  keep moraebot running in the background (macOS)
  moraebot run              or run it in this terminal instead
Then message the bot in its channel or in a DM. At its next start it says hello in the
channel and asks a few questions about you; reply skip to skip them.
";

const DEFAULT_WORK_DIR: &str = "~/moraebot-work";
const TOKEN_FILE_NAME: &str = "discord.token";
const SLACK_BOT_TOKEN_FILE_NAME: &str = "slack-bot.token";
const SLACK_APP_TOKEN_FILE_NAME: &str = "slack-app.token";

const SLACK_MANIFEST: &str = include_str!("../docs/guides/slack-manifest.json");
const SLACK_GUIDE: &str = include_str!("../docs/guides/slack.md");
const DISCORD_GUIDE: &str = include_str!("../docs/guides/discord.md");

const BAD_ID: &str = "Please enter a non-zero number.";
const BAD_SLACK_OWNER: &str =
    "Please enter a Slack member id: capital letters and digits, starting with U or W.";
const BAD_SLACK_CHANNEL: &str =
    "Please enter a Slack channel id: capital letters and digits, starting with C or G.";
const BAD_BOT_TOKEN: &str =
    "E_SLACK_TOKEN_KIND: a Slack bot token starts with xoxb-. Please enter it again.";
const BAD_APP_TOKEN: &str =
    "E_SLACK_TOKEN_KIND: a Slack app token starts with xapp-. Please enter it again.";

pub struct Messenger {
    pub name: &'static str,
    /// `None` while the messenger is not implemented yet.
    pub platform: Option<Platform>,
}

pub const MESSENGERS: [Messenger; 4] = [
    Messenger {
        name: "Discord",
        platform: Some(Platform::Discord),
    },
    Messenger {
        name: "Slack",
        platform: Some(Platform::Slack),
    },
    Messenger {
        name: "Telegram",
        platform: None,
    },
    Messenger {
        name: "WhatsApp",
        platform: None,
    },
];

/// A token typed by the owner and the file it is saved to once every question is answered.
type PendingToken = (PathBuf, Option<String>);

/// Raw answers for the Discord messenger. The bot token is deliberately not part of this.
#[derive(Debug, Clone)]
pub struct Answers {
    pub owner_id: String,
    pub channel_id: String,
    pub omp_path: String,
    pub work_dir: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitOutcome {
    Kept,
    Written { checks_ok: bool },
}

pub fn parse_id(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok().filter(|id| *id != 0)
}

/// Turns answers into a Config. The token file always lives under `<home>/secrets/`.
pub fn build_config(answers: &Answers, env: &Env) -> Result<Config, ConfigError> {
    let owner_id = parse_id(&answers.owner_id).ok_or_else(|| {
        ConfigError::new(
            E_OWNER_MISSING,
            "the owner user id must be a non-zero number",
        )
    })?;
    let channel_id = parse_id(&answers.channel_id).ok_or_else(|| {
        ConfigError::new(
            E_CHANNEL_MISSING,
            "the channel id must be a non-zero number",
        )
    })?;
    let discord = DiscordConfig {
        owner_id,
        channel_id,
        token_file: env.default_home()?.join("secrets").join(TOKEN_FILE_NAME),
    };
    assemble(
        env,
        Platform::Discord,
        Some(discord),
        None,
        &answers.omp_path,
        &answers.work_dir,
    )
}

/// Builds a fresh config around one messenger section.
fn assemble(
    env: &Env,
    messenger: Platform,
    discord: Option<DiscordConfig>,
    slack: Option<SlackConfig>,
    omp_path: &str,
    work_dir: &str,
) -> Result<Config, ConfigError> {
    let omp_path = omp_path.trim();
    if omp_path.is_empty() {
        return Err(ConfigError::new(
            E_OMP_MISSING,
            "the path to omp is required",
        ));
    }
    let work_dir = match work_dir.trim() {
        "" => DEFAULT_WORK_DIR,
        typed => typed,
    };
    Ok(Config {
        messenger,
        discord,
        slack,
        work_dir: env.expand_tilde(Path::new(work_dir))?,
        omp_path: env.expand_tilde(Path::new(omp_path))?,
        home: env.default_home()?,
        access_mode: AccessMode::OwnerOnly,
        omp: OmpConfig::default(),
    })
}

/// `moraebot guide <messenger>`: the same text as `docs/guides/<messenger>.md`.
pub fn guide(messenger: Platform) -> &'static str {
    match messenger {
        Platform::Discord => DISCORD_GUIDE,
        Platform::Slack => SLACK_GUIDE,
    }
}

/// Link that opens Slack's app creation page with the manifest already filled in.
pub fn slack_manifest_link() -> String {
    let manifest: serde_json::Value =
        serde_json::from_str(SLACK_MANIFEST).expect("the embedded manifest is valid JSON");
    format!(
        "https://api.slack.com/apps?new_app=1&manifest_json={}",
        percent_encode(&manifest.to_string())
    )
}

/// Keeps `A-Z a-z 0-9 - _ . ~` and writes every other UTF-8 byte as `%XX`.
fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len() * 2);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Slack ids are capital letters and digits; the first letter tells the kind.
fn valid_slack_id(id: &str, first: &[char]) -> bool {
    id.len() >= 2
        && id.starts_with(first)
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

pub fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn find_in_path(name: &str, path_var: Option<&OsString>) -> Option<PathBuf> {
    let path_var = path_var?;
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

struct Prompter<R, W> {
    input: R,
    out: W,
    is_tty: bool,
}

impl<R: BufRead, W: Write> Prompter<R, W> {
    /// End of input answers a question with a default by accepting it (an empty answer).
    fn read_line(&mut self, has_default: bool) -> Result<String, ConfigError> {
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            if !has_default {
                return Err(ConfigError::new(
                    E_INIT_EOF,
                    "input ended on a question that has no default",
                ));
            }
            writeln!(self.out)?;
            return Ok(String::new());
        }
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }

    fn read_answer(
        &mut self,
        prompt: &str,
        secret: bool,
        has_default: bool,
    ) -> Result<String, ConfigError> {
        write!(self.out, "{prompt}")?;
        self.out.flush()?;
        if secret && self.is_tty {
            let secret = rpassword::read_password()?;
            writeln!(self.out)?;
            return Ok(secret);
        }
        self.read_line(has_default)
    }

    fn ask_with_default(&mut self, prompt: &str) -> Result<String, ConfigError> {
        self.read_answer(prompt, false, true)
    }

    fn ask_until<T>(
        &mut self,
        prompt: &str,
        secret: bool,
        has_default: bool,
        mut parse: impl FnMut(&str) -> Result<T, &'static str>,
    ) -> Result<T, ConfigError> {
        loop {
            let line = self.read_answer(prompt, secret, has_default)?;
            match parse(&line) {
                Ok(value) => return Ok(value),
                Err(message) => writeln!(self.out, "{message}")?,
            }
        }
    }

    /// Asks for an id. Enter keeps `existing` when there is one.
    fn ask_id(
        &mut self,
        label: &str,
        existing: Option<String>,
        valid: impl Fn(&str) -> bool,
        bad: &'static str,
    ) -> Result<String, ConfigError> {
        let prompt = match &existing {
            Some(current) => format!("{label} [{current}]: "),
            None => format!("{label}: "),
        };
        self.ask_until(&prompt, false, existing.is_some(), |line| {
            let line = line.trim();
            if line.is_empty() {
                existing.clone().ok_or(bad)
            } else if valid(line) {
                Ok(line.to_string())
            } else {
                Err(bad)
            }
        })
    }

    /// Asks for a token without echo. Enter keeps the token already saved in `file`.
    /// `kind` is the required prefix and the message shown when it does not match.
    fn ask_token(
        &mut self,
        label: &str,
        file: &Path,
        kind: Option<(&'static str, &'static str)>,
    ) -> Result<PendingToken, ConfigError> {
        let saved = file.is_file();
        let prompt = if saved {
            format!("{label} (hidden) [Enter keeps the saved token]: ")
        } else {
            format!("{label} (hidden): ")
        };
        let token = self.ask_until(&prompt, true, saved, |line| {
            let token = line.trim();
            let token = token.strip_suffix(',').unwrap_or(token).trim();
            if token.is_empty() {
                return if saved {
                    Ok(None)
                } else {
                    Err("The token cannot be empty.")
                };
            }
            match kind {
                Some((prefix, message)) if !token.starts_with(prefix) => Err(message),
                _ => Ok(Some(token.to_string())),
            }
        })?;
        Ok((file.to_path_buf(), token))
    }

    fn ask_discord(
        &mut self,
        existing: Option<&DiscordConfig>,
        home: &Path,
    ) -> Result<(DiscordConfig, Vec<PendingToken>), ConfigError> {
        let owner_id = self.ask_id(
            "Discord owner user id",
            existing.map(|d| d.owner_id.to_string()),
            |id| parse_id(id).is_some(),
            BAD_ID,
        )?;
        let channel_id = self.ask_id(
            "Discord channel id",
            existing.map(|d| d.channel_id.to_string()),
            |id| parse_id(id).is_some(),
            BAD_ID,
        )?;
        let token_file = existing.map_or_else(
            || home.join("secrets").join(TOKEN_FILE_NAME),
            |d| d.token_file.clone(),
        );
        let token = self.ask_token("Discord bot token", &token_file, None)?;
        let section = DiscordConfig {
            owner_id: parse_id(&owner_id).expect("validated by ask_id"),
            channel_id: parse_id(&channel_id).expect("validated by ask_id"),
            token_file,
        };
        Ok((section, vec![token]))
    }

    fn ask_slack(
        &mut self,
        existing: Option<&SlackConfig>,
        home: &Path,
    ) -> Result<(SlackConfig, Vec<PendingToken>), ConfigError> {
        let owner_id = self.ask_id(
            "Slack owner member id (starts with U)",
            existing.map(|s| s.owner_id.clone()),
            |id| valid_slack_id(id, &['U', 'W']),
            BAD_SLACK_OWNER,
        )?;
        let channel_id = self.ask_id(
            "Slack channel id (starts with C or G)",
            existing.map(|s| s.channel_id.clone()),
            |id| valid_slack_id(id, &['C', 'G']),
            BAD_SLACK_CHANNEL,
        )?;
        let bot_token_file = existing.map_or_else(
            || home.join("secrets").join(SLACK_BOT_TOKEN_FILE_NAME),
            |s| s.bot_token_file.clone(),
        );
        let app_token_file = existing.map_or_else(
            || home.join("secrets").join(SLACK_APP_TOKEN_FILE_NAME),
            |s| s.app_token_file.clone(),
        );
        let bot = self.ask_token(
            "Slack bot token, xoxb-...",
            &bot_token_file,
            Some(("xoxb-", BAD_BOT_TOKEN)),
        )?;
        let app = self.ask_token(
            "Slack app token, xapp-...",
            &app_token_file,
            Some(("xapp-", BAD_APP_TOKEN)),
        )?;
        let section = SlackConfig {
            owner_id,
            channel_id,
            bot_token_file,
            app_token_file,
        };
        Ok((section, vec![bot, app]))
    }

    fn print_slack_link(&mut self) -> Result<(), ConfigError> {
        writeln!(
            self.out,
            "Create the Slack app with this link (the manifest is filled in):\n{}",
            slack_manifest_link()
        )?;
        Ok(())
    }

    /// Writes the config and the typed tokens, then runs the checks on what was written.
    fn finish(
        &mut self,
        config: &Config,
        config_path: &Path,
        env: &Env,
        tokens: Vec<PendingToken>,
    ) -> Result<InitOutcome, ConfigError> {
        config.ensure_home_layout()?;
        write_private_file(config_path, config.to_toml()?.as_bytes())?;
        writeln!(self.out, "Wrote {}", config_path.display())?;
        for (file, token) in tokens {
            if let Some(token) = token {
                write_private_file(&file, token.as_bytes())?;
                writeln!(self.out, "Wrote token file {} (mode 600)", file.display())?;
            }
        }
        writeln!(self.out, "Checking the new config:")?;
        let loaded = Config::load(config_path, env)?;
        let checks_ok = report_checks(&loaded.checks(), &mut self.out)?;
        Ok(InitOutcome::Written { checks_ok })
    }

    fn choose_messenger(&mut self) -> Result<Platform, ConfigError> {
        writeln!(self.out, "Choose a messenger:")?;
        for (index, messenger) in MESSENGERS.iter().enumerate() {
            let suffix = if messenger.platform.is_some() {
                ""
            } else {
                " (coming later)"
            };
            writeln!(self.out, "  {}) {}{suffix}", index + 1, messenger.name)?;
        }
        self.ask_until("Messenger [1]: ", false, true, |line| {
            let line = line.trim();
            let choice = if line.is_empty() {
                Some(1)
            } else {
                line.parse::<usize>().ok()
            };
            match choice.and_then(|n| MESSENGERS.get(n.wrapping_sub(1))) {
                Some(Messenger {
                    platform: Some(platform),
                    ..
                }) => Ok(*platform),
                Some(_) => Err("That messenger is coming later. Please choose an available one."),
                None => Err("Please enter one of the listed numbers."),
            }
        })
    }
}

pub fn run<R: BufRead, W: Write>(
    env: &Env,
    input: R,
    out: W,
    is_tty: bool,
) -> Result<InitOutcome, ConfigError> {
    let mut prompter = Prompter { input, out, is_tty };
    let config_path = env.default_config_path()?;

    if config_path.exists() {
        let prompt = format!(
            "A config already exists at {}. Overwrite it? [y/N]: ",
            config_path.display()
        );
        let answer = prompter.ask_with_default(&prompt)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            writeln!(prompter.out, "Keeping the existing config.")?;
            return Ok(InitOutcome::Kept);
        }
    }

    let messenger = prompter.choose_messenger()?;
    let home = env.default_home()?;
    let (discord, slack, tokens) = match messenger {
        Platform::Discord => {
            let (section, tokens) = prompter.ask_discord(None, &home)?;
            (Some(section), None, tokens)
        }
        Platform::Slack => {
            prompter.print_slack_link()?;
            let (section, tokens) = prompter.ask_slack(None, &home)?;
            (None, Some(section), tokens)
        }
    };

    let omp_default = find_in_path("omp", env.path.as_ref());
    let omp_prompt = match &omp_default {
        Some(found) => format!("Path to omp [{}]: ", found.display()),
        None => "Path to omp (not found on PATH): ".to_string(),
    };
    let omp_path = prompter.ask_until(&omp_prompt, false, omp_default.is_some(), |line| {
        let line = line.trim();
        if line.is_empty() {
            return omp_default.clone().ok_or("Please enter the path to omp.");
        }
        let candidate = env
            .expand_tilde(Path::new(line))
            .map_err(|_| "Cannot expand '~' because HOME is not set.")?;
        if is_executable_file(&candidate) {
            Ok(candidate)
        } else {
            Err("That path is not an executable file.")
        }
    })?;
    let work_dir = prompter.ask_with_default(&format!("Work folder [{DEFAULT_WORK_DIR}]: "))?;

    let config = assemble(
        env,
        messenger,
        discord,
        slack,
        &omp_path.to_string_lossy(),
        &work_dir,
    )?;
    let outcome = prompter.finish(&config, &config_path, env, tokens)?;
    request(&config.home).map_err(|e| ConfigError::new(E_ONBOARDING_REQUEST, e.detail))?;
    if outcome == (InitOutcome::Written { checks_ok: true }) {
        write!(prompter.out, "{NEXT_STEPS}")?;
    }
    Ok(outcome)
}

/// `moraebot setup <messenger>`: asks only that messenger's required fields, keeps every
/// other config value and makes the messenger the active one.
pub fn setup<R: BufRead, W: Write>(
    env: &Env,
    messenger: Platform,
    input: R,
    out: W,
    is_tty: bool,
) -> Result<InitOutcome, ConfigError> {
    let mut prompter = Prompter { input, out, is_tty };
    let config_path = env.default_config_path()?;
    if !config_path.exists() {
        return Err(ConfigError::new(
            E_SETUP_NO_CONFIG,
            format!(
                "there is no config at {}; run `moraebot init` first",
                config_path.display()
            ),
        ));
    }
    let mut config = Config::load(&config_path, env)?;
    let tokens = match messenger {
        Platform::Discord => {
            let (section, tokens) = prompter.ask_discord(config.discord.as_ref(), &config.home)?;
            config.discord = Some(section);
            tokens
        }
        Platform::Slack => {
            prompter.print_slack_link()?;
            let (section, tokens) = prompter.ask_slack(config.slack.as_ref(), &config.home)?;
            config.slack = Some(section);
            tokens
        }
    };
    config.messenger = messenger;
    let outcome = prompter.finish(&config, &config_path, env, tokens)?;
    writeln!(
        prompter.out,
        "{} is now the active messenger. Restart moraebot to use it.",
        messenger.as_str()
    )?;
    Ok(outcome)
}

/// Replaces `path` with a new file readable only by the owner.
fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    let io_err =
        |e: io::Error| ConfigError::new(E_IO, format!("cannot write {}: {e}", path.display()));
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(e)),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_err)?;
    file.write_all(contents).map_err(io_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture {
        dir: TempDir,
        env: Env,
        omp: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let omp = dir.path().join("omp");
            fs::write(&omp, "#!/bin/sh\n").expect("write omp");
            fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).expect("chmod omp");
            let env = Env {
                home_dir: Some(dir.path().to_path_buf()),
                moraebot_home: Some(dir.path().join("mhome")),
                path: None,
            };
            Self { dir, env, omp }
        }

        fn answers(&self) -> Answers {
            Answers {
                owner_id: "111".into(),
                channel_id: "222".into(),
                omp_path: self.omp.to_string_lossy().into_owned(),
                work_dir: String::new(),
            }
        }

        fn config_path(&self) -> PathBuf {
            self.env.default_config_path().expect("config path")
        }

        fn run(&self, input: &str) -> (Result<InitOutcome, ConfigError>, String) {
            let mut out = Vec::new();
            let result = run(&self.env, input.as_bytes(), &mut out, false);
            (result, String::from_utf8(out).expect("utf-8 output"))
        }

        fn happy_input(&self) -> String {
            format!(
                "1\n111\n222\nTOKEN_SENTINEL_9f3a\n{}\n\n",
                self.omp.display()
            )
        }
    }

    fn code<T: std::fmt::Debug>(result: Result<T, ConfigError>) -> &'static str {
        result.expect_err("expected an error").code
    }

    #[test]
    fn build_config_maps_answers() {
        let fx = Fixture::new();
        let cfg = build_config(&fx.answers(), &fx.env).expect("builds");
        let discord = cfg.discord.as_ref().expect("discord section");
        assert_eq!(cfg.owner_id(), "111");
        assert_eq!(discord.channel_id, 222);
        assert_eq!(cfg.omp_path, fx.omp);
        assert_eq!(cfg.home, fx.dir.path().join("mhome"));
        assert_eq!(
            discord.token_file,
            fx.dir.path().join("mhome/secrets/discord.token")
        );
        assert_eq!(cfg.work_dir, fx.dir.path().join("moraebot-work"));
        assert_eq!(cfg.omp.turn_timeout_minutes, None);
        assert_eq!(cfg.omp.max_cost_usd, None);
    }

    #[test]
    fn build_config_keeps_a_typed_work_dir() {
        let fx = Fixture::new();
        let answers = Answers {
            work_dir: "~/elsewhere".into(),
            ..fx.answers()
        };
        let cfg = build_config(&answers, &fx.env).expect("builds");
        assert_eq!(cfg.work_dir, fx.dir.path().join("elsewhere"));
    }

    #[test]
    fn build_config_rejects_bad_answers() {
        let fx = Fixture::new();
        for owner in ["", "0", "abc", "-5"] {
            let answers = Answers {
                owner_id: owner.into(),
                ..fx.answers()
            };
            assert_eq!(
                code(build_config(&answers, &fx.env)),
                E_OWNER_MISSING,
                "owner {owner:?}"
            );
        }
        let answers = Answers {
            channel_id: "0".into(),
            ..fx.answers()
        };
        assert_eq!(code(build_config(&answers, &fx.env)), E_CHANNEL_MISSING);
        let answers = Answers {
            omp_path: "  ".into(),
            ..fx.answers()
        };
        assert_eq!(code(build_config(&answers, &fx.env)), E_OMP_MISSING);
    }

    #[test]
    fn built_config_passes_validation() {
        let fx = Fixture::new();
        let cfg = build_config(&fx.answers(), &fx.env).expect("builds");
        cfg.ensure_home_layout().expect("layout");
        let token_file = cfg
            .discord
            .as_ref()
            .expect("discord section")
            .token_file
            .clone();
        write_private_file(&token_file, b"x").expect("token");
        cfg.validate().expect("valid");
    }

    #[test]
    fn find_in_path_returns_first_executable() {
        let fx = Fixture::new();
        let plain = fx.dir.path().join("plain");
        fs::create_dir(&plain).expect("mkdir");
        fs::write(plain.join("omp"), "").expect("write non-executable");
        let path = std::env::join_paths([plain, fx.dir.path().to_path_buf()]).expect("join");
        assert_eq!(find_in_path("omp", Some(&path)), Some(fx.omp.clone()));
        assert_eq!(find_in_path("missing", Some(&path)), None);
        assert_eq!(find_in_path("omp", None), None);
    }

    #[test]
    fn run_writes_valid_config_and_private_token() {
        let fx = Fixture::new();
        let (result, out) = fx.run(&fx.happy_input());
        assert_eq!(
            result.expect("init runs"),
            InitOutcome::Written { checks_ok: true }
        );

        let token_path = fx.dir.path().join("mhome/secrets/discord.token");
        let mode = fs::metadata(&token_path)
            .expect("token file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            fs::read_to_string(&token_path).expect("read token"),
            "TOKEN_SENTINEL_9f3a"
        );

        let config_text = fs::read_to_string(fx.config_path()).expect("config");
        let config_mode = fs::metadata(fx.config_path())
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(config_mode, 0o600);
        assert!(!config_text.contains("TOKEN_SENTINEL_9f3a"));
        assert!(!out.contains("TOKEN_SENTINEL_9f3a"));
        let loaded = Config::load(&fx.config_path(), &fx.env).expect("loads");
        loaded.validate().expect("valid");
        assert_eq!(loaded.owner_id(), "111");
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn run_requests_onboarding_and_prints_next_steps() {
        let fx = Fixture::new();
        let (result, out) = fx.run(&fx.happy_input());
        assert_eq!(
            result.expect("init runs"),
            InitOutcome::Written { checks_ok: true }
        );
        assert!(out.ends_with(NEXT_STEPS));
        let request = crate::onboarding::request_file(&fx.dir.path().join("mhome"));
        assert_eq!(mode_of(&request), 0o600);
    }

    #[test]
    fn kept_config_requests_nothing() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.config_path().parent().expect("parent")).expect("mkdir");
        fs::write(fx.config_path(), "original").expect("write");
        let (result, out) = fx.run("n\n");
        assert_eq!(result.expect("init runs"), InitOutcome::Kept);
        assert!(!out.contains(NEXT_STEPS));
        assert!(!crate::onboarding::request_file(&fx.dir.path().join("mhome")).exists());
    }

    #[test]
    fn unavailable_messenger_is_asked_again() {
        let fx = Fixture::new();
        let input = format!(
            "3\n9\n1\n111\n222\nTOKEN_SENTINEL_9f3a\n{}\n\n",
            fx.omp.display()
        );
        let (result, _) = fx.run(&input);
        assert_eq!(
            result.expect("init runs"),
            InitOutcome::Written { checks_ok: true }
        );
        assert_eq!(
            Config::load(&fx.config_path(), &fx.env)
                .expect("loads")
                .owner_id(),
            "111"
        );
    }

    #[test]
    fn invalid_ids_and_omp_path_are_asked_again() {
        let fx = Fixture::new();
        let missing = fx.dir.path().join("no-such-omp");
        let input = format!(
            "1\nabc\n0\n111\nxyz\n222\n\nTOKEN_SENTINEL_9f3a\n{}\n{}\n\n",
            missing.display(),
            fx.omp.display()
        );
        let (result, _) = fx.run(&input);
        assert_eq!(
            result.expect("init runs"),
            InitOutcome::Written { checks_ok: true }
        );
        let loaded = Config::load(&fx.config_path(), &fx.env).expect("loads");
        let discord = loaded.discord.as_ref().expect("discord section");
        assert_eq!(
            (loaded.owner_id(), discord.channel_id),
            ("111".to_string(), 222)
        );
    }

    #[test]
    fn existing_config_is_kept_by_default() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.config_path().parent().expect("parent")).expect("mkdir");
        fs::write(fx.config_path(), "original").expect("write");
        let (result, _) = fx.run("\n");
        assert_eq!(result.expect("init runs"), InitOutcome::Kept);
        assert_eq!(
            fs::read_to_string(fx.config_path()).expect("read"),
            "original"
        );
    }

    #[test]
    fn existing_config_is_replaced_after_yes() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.config_path().parent().expect("parent")).expect("mkdir");
        fs::write(fx.config_path(), "original").expect("write");
        let (result, _) = fx.run(&format!("y\n{}", fx.happy_input()));
        assert_eq!(
            result.expect("init runs"),
            InitOutcome::Written { checks_ok: true }
        );
        assert!(Config::load(&fx.config_path(), &fx.env).is_ok());
    }

    #[test]
    fn end_of_input_accepts_defaults() {
        let fx = Fixture::new();
        let env = Env {
            path: Some(fx.dir.path().as_os_str().to_owned()),
            ..fx.env.clone()
        };
        let mut out = Vec::new();
        let input = "1\n111\n222\nTOKEN_SENTINEL_9f3a\n";
        let result = run(&env, input.as_bytes(), &mut out, false);
        assert_eq!(
            result.expect("init runs"),
            InitOutcome::Written { checks_ok: true }
        );
        let loaded = Config::load(&fx.config_path(), &env).expect("loads");
        assert_eq!(loaded.omp_path, fx.omp);
        assert_eq!(loaded.work_dir, fx.dir.path().join("moraebot-work"));
    }

    #[test]
    fn end_of_input_on_overwrite_question_keeps_the_config() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.config_path().parent().expect("parent")).expect("mkdir");
        fs::write(fx.config_path(), "original").expect("write");
        let (result, _) = fx.run("");
        assert_eq!(result.expect("init runs"), InitOutcome::Kept);
    }

    #[test]
    fn end_of_input_without_a_default_is_an_error() {
        let fx = Fixture::new();
        for input in [
            "",
            "1\n",
            "1\n111\n",
            "1\n111\n222\n",
            "1\n111\n222\nTOKEN_SENTINEL_9f3a\n",
        ] {
            let (result, _) = fx.run(input);
            assert_eq!(code(result), E_INIT_EOF, "input {input:?}");
            assert!(!fx.config_path().exists());
        }
    }

    #[test]
    fn ending_input_early_is_an_error() {
        let fx = Fixture::new();
        let (result, _) = fx.run("1\n111\n");
        assert_eq!(code(result), E_INIT_EOF);
        assert!(!fx.config_path().exists());
    }

    const BOT: &str = "xoxb-test-SENTINEL1";
    const APP: &str = "xapp-test-SENTINEL2";

    impl Fixture {
        fn setup(
            &self,
            messenger: Platform,
            input: &str,
        ) -> (Result<InitOutcome, ConfigError>, String) {
            let mut out = Vec::new();
            let result = setup(&self.env, messenger, input.as_bytes(), &mut out, false);
            (result, String::from_utf8(out).expect("utf-8 output"))
        }

        fn secret(&self, name: &str) -> PathBuf {
            self.dir.path().join("mhome/secrets").join(name)
        }

        fn slack_input(&self) -> String {
            format!("U111\nC222\n{BOT}\n{APP}\n")
        }
    }

    #[test]
    fn init_slack_writes_slack_section_and_two_private_tokens() {
        let fx = Fixture::new();
        let input = format!("2\nU111\nC222\n{BOT}\n{APP}\n{}\n\n", fx.omp.display());
        let (result, out) = fx.run(&input);
        assert_eq!(
            result.expect("init runs"),
            InitOutcome::Written { checks_ok: true }
        );
        for name in ["slack-bot.token", "slack-app.token"] {
            let mode = fs::metadata(fx.secret(name))
                .expect("token")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name}");
        }
        assert_eq!(
            fs::read_to_string(fx.secret("slack-bot.token")).expect("bot"),
            BOT
        );
        assert_eq!(
            fs::read_to_string(fx.secret("slack-app.token")).expect("app"),
            APP
        );
        let config_text = fs::read_to_string(fx.config_path()).expect("config");
        assert!(config_text.contains("messenger = \"slack\""));
        for text in [&config_text, &out] {
            assert!(!text.contains("SENTINEL"));
        }
        let loaded = Config::load(&fx.config_path(), &fx.env).expect("loads");
        assert_eq!(loaded.messenger, Platform::Slack);
        assert!(loaded.discord.is_none());
    }

    #[test]
    fn setup_slack_keeps_discord_section_and_work_dir() {
        let fx = Fixture::new();
        let typed_work = fx.dir.path().join("typed-work");
        let input = format!(
            "1\n111\n222\nTOKEN_SENTINEL_9f3a\n{}\n{}\n",
            fx.omp.display(),
            typed_work.display()
        );
        assert!(fx.run(&input).0.is_ok());
        let (result, out) = fx.setup(Platform::Slack, &fx.slack_input());
        assert_eq!(
            result.expect("setup runs"),
            InitOutcome::Written { checks_ok: true }
        );
        assert!(out.contains("slack is now the active messenger"));
        let loaded = Config::load(&fx.config_path(), &fx.env).expect("loads");
        assert_eq!(loaded.messenger, Platform::Slack);
        assert_eq!(loaded.work_dir, typed_work);
        let discord = loaded.discord.as_ref().expect("discord kept");
        assert_eq!((discord.owner_id, discord.channel_id), (111, 222));
        assert_eq!(loaded.slack.as_ref().expect("slack").owner_id, "U111");
        assert_eq!(
            fs::read_to_string(fx.secret("discord.token")).expect("discord token"),
            "TOKEN_SENTINEL_9f3a"
        );
    }

    #[test]
    fn setup_discord_on_legacy_config_rewrites_new_layout() {
        let fx = Fixture::new();
        let secrets = fx.dir.path().join("mhome/secrets");
        fs::create_dir_all(&secrets).expect("mkdir");
        let token = secrets.join("discord.token");
        write_private_file(&token, b"saved").expect("token");
        let legacy = format!(
            "owner_id = 111\ntoken_file = {:?}\nomp_path = {:?}\n[discord]\nchannel_id = 222\n",
            token.display().to_string(),
            fx.omp.display().to_string()
        );
        fs::write(fx.config_path(), legacy).expect("legacy config");
        let (result, _) = fx.setup(Platform::Discord, "\n\n\n");
        assert_eq!(
            result.expect("setup runs"),
            InitOutcome::Written { checks_ok: true }
        );
        let text = fs::read_to_string(fx.config_path()).expect("config");
        assert!(
            !text
                .split("[discord]")
                .next()
                .expect("head")
                .contains("owner_id")
        );
        let loaded = Config::load(&fx.config_path(), &fx.env).expect("loads");
        assert_eq!(loaded.discord.as_ref().expect("discord").owner_id, 111);
        assert_eq!(fs::read_to_string(&token).expect("token"), "saved");
    }

    #[test]
    fn setup_keeps_saved_tokens_on_enter() {
        let fx = Fixture::new();
        assert!(fx.setup_after_init_slack().is_ok());
        let (result, out) = fx.setup(Platform::Slack, "U222\n\n\n\n");
        assert!(result.is_ok());
        assert!(out.contains("Enter keeps the saved token"));
        assert_eq!(
            fs::read_to_string(fx.secret("slack-bot.token")).expect("bot"),
            BOT
        );
        assert_eq!(
            fs::read_to_string(fx.secret("slack-app.token")).expect("app"),
            APP
        );
        let loaded = Config::load(&fx.config_path(), &fx.env).expect("loads");
        let slack = loaded.slack.as_ref().expect("slack");
        assert_eq!(
            (slack.owner_id.as_str(), slack.channel_id.as_str()),
            ("U222", "C222")
        );
    }

    impl Fixture {
        fn setup_after_init_slack(&self) -> Result<InitOutcome, ConfigError> {
            let input = format!("2\nU111\nC222\n{BOT}\n{APP}\n{}\n\n", self.omp.display());
            self.run(&input).0
        }
    }

    #[test]
    fn setup_requests_nothing() {
        let fx = Fixture::new();
        assert!(fx.setup_after_init_slack().is_ok());
        let request = crate::onboarding::request_file(&fx.dir.path().join("mhome"));
        fs::remove_file(&request).expect("init left a request");
        let (result, out) = fx.setup(Platform::Slack, "U222\n\n\n\n");
        assert_eq!(
            result.expect("setup runs"),
            InitOutcome::Written { checks_ok: true }
        );
        assert!(!request.exists());
        assert!(!out.contains(NEXT_STEPS));
    }

    #[test]
    fn wrong_token_prefix_is_asked_again() {
        let fx = Fixture::new();
        assert!(fx.run(&fx.happy_input()).0.is_ok());
        let input = format!("U111\nC222\n{APP}\n{BOT}\n{APP}\n");
        let (result, out) = fx.setup(Platform::Slack, &input);
        assert!(result.is_ok());
        assert!(out.contains(E_SLACK_TOKEN_KIND));
        assert_eq!(
            fs::read_to_string(fx.secret("slack-bot.token")).expect("bot"),
            BOT
        );
    }

    #[test]
    fn failed_setup_leaves_the_config_untouched() {
        let fx = Fixture::new();
        assert!(fx.run(&fx.happy_input()).0.is_ok());
        let before = fs::read(fx.config_path()).expect("config");
        let (result, _) = fx.setup(Platform::Slack, &format!("U111\nC222\n{APP}\n"));
        assert_eq!(code(result), E_INIT_EOF);
        assert_eq!(fs::read(fx.config_path()).expect("config"), before);
        assert!(!fx.secret("slack-bot.token").exists());
    }

    #[test]
    fn setup_without_config_is_setup_no_config() {
        let fx = Fixture::new();
        let (result, _) = fx.setup(Platform::Slack, &fx.slack_input());
        assert_eq!(code(result), E_SETUP_NO_CONFIG);
    }

    #[test]
    fn slack_ids_need_the_right_prefix() {
        assert!(valid_slack_id("U111", &['U', 'W']));
        assert!(valid_slack_id("C222", &['C', 'G']));
        assert!(!valid_slack_id("111", &['U', 'W']));
        assert!(!valid_slack_id("u111", &['U', 'W']));
        assert!(!valid_slack_id("C2 2", &['C', 'G']));
    }

    #[test]
    fn percent_encode_keeps_unreserved_only() {
        assert_eq!(
            percent_encode("a-b_c.d~E9 {\"é"),
            "a-b_c.d~E9%20%7B%22%C3%A9"
        );
    }
}
