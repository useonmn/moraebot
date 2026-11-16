//! Interactive `moraebot init`.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::config::{
    report_checks, AccessMode, Config, ConfigError, DiscordConfig, Env, OmpConfig, Platform,
    E_CHANNEL_MISSING, E_IO, E_OMP_MISSING, E_OWNER_MISSING,
};

pub const E_INIT_EOF: &str = "E_INIT_EOF";

const DEFAULT_WORK_DIR: &str = "~/moraebot-work";
const TOKEN_FILE_NAME: &str = "discord.token";

pub struct Messenger {
    pub name: &'static str,
    pub available: bool,
}

pub const MESSENGERS: [Messenger; 4] = [
    Messenger {
        name: "Discord",
        available: true,
    },
    Messenger {
        name: "Slack",
        available: false,
    },
    Messenger {
        name: "Telegram",
        available: false,
    },
    Messenger {
        name: "WhatsApp",
        available: false,
    },
];

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
    let omp_path = answers.omp_path.trim();
    if omp_path.is_empty() {
        return Err(ConfigError::new(
            E_OMP_MISSING,
            "the path to omp is required",
        ));
    }
    let work_dir = match answers.work_dir.trim() {
        "" => DEFAULT_WORK_DIR,
        typed => typed,
    };
    let home = env.default_home()?;
    Ok(Config {
        messenger: Platform::Discord,
        discord: Some(DiscordConfig {
            owner_id,
            channel_id,
            token_file: home.join("secrets").join(TOKEN_FILE_NAME),
        }),
        slack: None,
        work_dir: env.expand_tilde(Path::new(work_dir))?,
        omp_path: env.expand_tilde(Path::new(omp_path))?,
        home,
        access_mode: AccessMode::OwnerOnly,
        omp: OmpConfig::default(),
    })
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

    fn choose_messenger(&mut self) -> Result<(), ConfigError> {
        writeln!(self.out, "Choose a messenger:")?;
        for (index, messenger) in MESSENGERS.iter().enumerate() {
            let suffix = if messenger.available {
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
                Some(messenger) if messenger.available => Ok(()),
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

    prompter.choose_messenger()?;
    let owner_id = prompter.ask_until("Discord owner user id: ", false, false, |line| {
        parse_id(line)
            .map(|_| line.trim().to_string())
            .ok_or("Please enter a non-zero number.")
    })?;
    let channel_id = prompter.ask_until("Discord channel id: ", false, false, |line| {
        parse_id(line)
            .map(|_| line.trim().to_string())
            .ok_or("Please enter a non-zero number.")
    })?;
    let token = prompter.ask_until("Discord bot token (hidden): ", true, false, |line| {
        let token = line.trim();
        if token.is_empty() {
            Err("The token cannot be empty.")
        } else {
            Ok(token.to_string())
        }
    })?;

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

    let config = build_config(
        &Answers {
            owner_id,
            channel_id,
            omp_path: omp_path.to_string_lossy().into_owned(),
            work_dir,
        },
        env,
    )?;
    config.ensure_home_layout()?;
    let token_file = config
        .discord
        .as_ref()
        .map(|d| d.token_file.clone())
        .expect("build_config sets the discord section");
    write_private_file(&token_file, token.as_bytes())?;
    write_private_file(&config_path, config.to_toml()?.as_bytes())?;
    writeln!(prompter.out, "Wrote {}", config_path.display())?;
    writeln!(
        prompter.out,
        "Wrote token file {} (mode 600)",
        token_file.display()
    )?;

    writeln!(prompter.out, "Checking the new config:")?;
    let loaded = Config::load(&config_path, env)?;
    let checks_ok = report_checks(&loaded.checks(), &mut prompter.out)?;
    Ok(InitOutcome::Written { checks_ok })
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
        let token_file = cfg.discord.as_ref().expect("discord section").token_file.clone();
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

    #[test]
    fn unavailable_messenger_is_asked_again() {
        let fx = Fixture::new();
        let input = format!(
            "2\n9\n1\n111\n222\nTOKEN_SENTINEL_9f3a\n{}\n\n",
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
        assert_eq!((loaded.owner_id(), discord.channel_id), ("111".to_string(), 222));
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
}
