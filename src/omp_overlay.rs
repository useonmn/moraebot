//! omp's own memory is off for the lead and every lane: both run with
//! `--config <home>/omp-overlay.yml` ([`OMP_OVERLAY`]), so moraebot's memory store
//! (`<home>/memory`) does not mix with the owner's terminal omp memory (#39).
//!
//! omp deep-merges a `--config` file per key over the owner's global and project settings
//! (precedence: runtime override, `--config`, project, global), so the overlay changes only
//! the keys it names; the owner's models and providers stay as they are. Checked against
//! omp 18.22.2 (`packages/coding-agent/src/config/settings.ts`, `#mergeOwnLayers` and
//! `getProvenance`).
//!
//! moraebot cannot read the settings omp ends up with: `omp config` loads no `--config`
//! overlay and the RPC `get_state` reports no settings. So [`check`] runs at start and
//! - refuses (`E_OMP_OVERLAY`) when omp does not know a key the overlay sets, because omp
//!   ignores an unknown key and its memory would come back on;
//! - warns when omp is not the version the merge was checked against ([`CHECKED_OMP_VERSION`]).
//!
//! The file name differs from the 0.2.0 `omp/config-overlay.yml`, which still refuses start
//! (`memory::check_old_layout`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::ConfigError;

/// omp does not know a setting the overlay sets, or cannot be asked.
pub const E_OMP_OVERLAY: &str = "E_OMP_OVERLAY";
/// omp is not the version the overlay merge was checked against.
pub const W_OMP_VERSION: &str = "W_OMP_VERSION";

/// The omp version whose `--config` merge the overlay was checked against. It also stands for
/// a second fact: a one-line `--append-system-prompt` value is read as a file, once, at start
/// (`resolvePromptInput`, `system-prompt.ts`), which keeps the lead's and the lanes' memory
/// out of argv (`omp::write_append_file`). An omp upgrade means re-checking both.
pub const CHECKED_OMP_VERSION: &str = "18.22.2";

/// The overlay of the lead and lane children. Same memory keys as the chat overlay
/// (`chat::CHAT_OVERLAY`), nothing else. `off` is quoted so no YAML reader takes it for false.
pub const OMP_OVERLAY: &str = "\
# Written by moraebot for the lead and lanes. Edits are overwritten.
memory:
  backend: \"off\"
autolearn:
  enabled: false
";

/// The settings [`OMP_OVERLAY`] sets, as `omp config get` names them.
pub const OVERLAY_KEYS: [&str; 2] = ["memory.backend", "autolearn.enabled"];

/// `<home>/omp-overlay.yml`.
pub fn overlay_path(home: &Path) -> PathBuf {
    home.join("omp-overlay.yml")
}

/// Numbers the temp files of [`write_overlay`] within this process.
static WRITE_TMP: AtomicU64 = AtomicU64::new(0);

/// Writes [`OMP_OVERLAY`] through a temp file of its own renamed over it, so a child starting
/// meanwhile never reads a partial file and two starts at once (the lead and a lane) do not
/// take each other's temp file. Called before every lead and lane start.
pub fn write_overlay(home: &Path) -> Result<(), ConfigError> {
    let path = overlay_path(home);
    let n = WRITE_TMP.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("yml.{}.{n}.tmp", std::process::id()));
    std::fs::write(&temp, OMP_OVERLAY)
        .and_then(|()| std::fs::rename(&temp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            ConfigError::io(
                E_OMP_OVERLAY,
                &format!("cannot write {}", path.display()),
                &e,
            )
        })
}

/// What [`check`] found: omp's version and, when it is not [`CHECKED_OMP_VERSION`], a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayCheck {
    pub version: String,
    pub warning: Option<String>,
}

/// Asks `omp` whether it knows every [`OVERLAY_KEYS`] setting (`omp config get <key> --json`)
/// and which version it is (`omp --version`). No model is called. omp runs in `home`, the
/// service's working folder, so `run` and `check-config` get the same answer: omp loads the
/// project settings of its working folder, and one it cannot load fails every `config get`.
pub fn check(omp: &Path, home: &Path) -> Result<OverlayCheck, ConfigError> {
    for key in OVERLAY_KEYS {
        let output = run(omp, home, &["config", "get", key, "--json"])?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() && !stderr.contains(&format!("Unknown setting: {key}")) {
            // omp failed for another reason (e.g. a settings file it cannot load); that says
            // nothing about the key, so report omp's own error instead.
            return Err(ConfigError::new(
                E_OMP_OVERLAY,
                format!(
                    "`{} config get {key}` failed in {}: {}",
                    omp.display(),
                    home.display(),
                    error_line(&stderr)
                ),
            ));
        }
        let known = output.status.success()
            && serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .is_ok_and(|v| v["key"] == key);
        if !known {
            return Err(ConfigError::new(
                E_OMP_OVERLAY,
                format!(
                    "{} does not know the setting {key}, so the memory overlay would be ignored and omp's own memory could run; update moraebot for this omp version",
                    omp.display()
                ),
            ));
        }
    }
    let output = run(omp, home, &["--version"])?;
    let version = parse_version(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        ConfigError::new(
            E_OMP_OVERLAY,
            format!("{} --version printed no version", omp.display()),
        )
    })?;
    let warning = (version != CHECKED_OMP_VERSION).then(|| {
        format!(
            "{W_OMP_VERSION}: omp {version} is not {CHECKED_OMP_VERSION}, the version the memory overlay was checked against; check that `--config` still merges per key, that memory.backend off still turns omp's memory off, and that a one-line --append-system-prompt value is still read as a file once at start"
        )
    });
    Ok(OverlayCheck { version, warning })
}

/// The line of omp's stderr that names the error: the first `error:` line (omp prints a
/// source excerpt before it), else the last non-empty line.
fn error_line(stderr: &str) -> &str {
    let mut lines = stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    lines
        .clone()
        .find(|l| l.to_ascii_lowercase().starts_with("error:"))
        .or_else(|| lines.next_back())
        .unwrap_or("no error output")
}

/// Runs `omp args` in `dir` with the same filtered environment as every omp child, so the
/// check never passes a messenger token or the jev key.
fn run(omp: &Path, dir: &Path, args: &[&str]) -> Result<std::process::Output, ConfigError> {
    Command::new(omp)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(crate::omp::check_env(&[]))
        .output()
        .map_err(|e| {
            ConfigError::io(
                E_OMP_OVERLAY,
                &format!(
                    "cannot run {} {} in {}",
                    omp.display(),
                    args.join(" "),
                    dir.display()
                ),
                &e,
            )
        })
}

/// `omp/18.22.2` (first line of `omp --version`) -> `18.22.2`.
fn parse_version(stdout: &str) -> Option<String> {
    let first = stdout.lines().next()?.trim();
    let version = first.strip_prefix("omp/").unwrap_or(first);
    let version = version.strip_prefix('v').unwrap_or(version);
    (!version.is_empty() && version.starts_with(|c: char| c.is_ascii_digit())).then(|| {
        version
            .split_whitespace()
            .next()
            .unwrap_or(version)
            .to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in omp: answers `--version` with `version` and `config get <key> --json` like
    /// omp 18.22.2, except for `unknown`, which it reports as an unknown setting (exit 1).
    fn fake_omp(dir: &Path, version: &str, unknown: &str) -> PathBuf {
        let path = dir.join("omp");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = --version ]; then echo 'omp/{version}'; exit 0; fi\n\
             if [ \"$1\" = config ] && [ \"$2\" = get ]; then\n\
               if [ \"$3\" = '{unknown}' ]; then echo \"Unknown setting: $3\" >&2; exit 1; fi\n\
               printf '{{\"key\": \"%s\", \"value\": null}}\\n' \"$3\"; exit 0\n\
             fi\n\
             exit 2\n"
        );
        std::fs::write(&path, script).expect("write fake omp");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[test]
    fn overlay_sets_exactly_the_checked_keys() {
        // The start check asks omp about the keys the overlay sets; a key added to one and
        // not the other would go unchecked.
        let mut set = Vec::new();
        let mut group = "";
        for line in OMP_OVERLAY.lines().filter(|l| !l.starts_with('#')) {
            match line.strip_prefix("  ") {
                Some(leaf) => {
                    let (name, value) = leaf.split_once(':').expect("key: value");
                    set.push((format!("{group}.{name}"), value.trim().to_owned()));
                }
                None => group = line.trim_end_matches(':'),
            }
        }
        assert_eq!(
            set,
            [
                ("memory.backend".to_owned(), "\"off\"".to_owned()),
                ("autolearn.enabled".to_owned(), "false".to_owned()),
            ]
        );
        assert_eq!(
            set.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            OVERLAY_KEYS
        );
    }

    #[test]
    fn write_overlay_replaces_an_edited_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(overlay_path(dir.path()), "memory:\n  backend: local\n").expect("edit");
        write_overlay(dir.path()).expect("written");
        let text = std::fs::read_to_string(overlay_path(dir.path())).expect("read");
        assert_eq!(text, OMP_OVERLAY);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["omp-overlay.yml"], "no temp file is left");
    }

    #[test]
    fn checked_version_passes_without_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let omp = fake_omp(dir.path(), CHECKED_OMP_VERSION, "");
        assert_eq!(
            check(&omp, dir.path()).expect("passes"),
            OverlayCheck {
                version: CHECKED_OMP_VERSION.to_owned(),
                warning: None,
            }
        );
    }

    #[test]
    fn other_version_passes_with_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let omp = fake_omp(dir.path(), "19.0.1", "");
        let found = check(&omp, dir.path()).expect("passes");
        assert_eq!(found.version, "19.0.1");
        assert!(found.warning.is_some_and(|w| w.starts_with(W_OMP_VERSION)));
    }

    #[test]
    fn unknown_overlay_key_refuses_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        for key in OVERLAY_KEYS {
            let omp = fake_omp(dir.path(), CHECKED_OMP_VERSION, key);
            let err = check(&omp, dir.path()).expect_err("refused");
            assert_eq!(err.code, E_OMP_OVERLAY, "{key}");
            assert!(err.to_string().contains("does not know the setting"), "{err}");
        }
    }

    #[test]
    fn check_runs_in_home_and_tells_a_failed_load_from_an_unknown_key() {
        // Like omp, the stand-in loads `<cwd>/.omp/config.yml` before it answers and fails
        // every `config get` when that file is broken. It also fails anywhere but `home`.
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".omp")).expect("home");
        let path = dir.path().join("omp");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$(pwd -P)\" != '{home}' ]; then echo \"error: wrong cwd $(pwd)\" >&2; exit 1; fi\n\
             if [ -f .omp/config.yml ]; then\n\
               echo '2755 | throw new Error(' >&2\n\
               echo 'error: Settings config is invalid: .omp/config.yml' >&2\n\
               echo '  at #load (settings.ts:2318:25)' >&2\n\
               exit 1\n\
             fi\n\
             if [ \"$1\" = --version ]; then echo 'omp/{CHECKED_OMP_VERSION}'; exit 0; fi\n\
             printf '{{\"key\": \"%s\", \"value\": null}}\\n' \"$3\"\n",
            home = home.canonicalize().expect("canonical home").display()
        );
        std::fs::write(&path, script).expect("write fake omp");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        assert!(check(&path, &home).expect("passes in home").warning.is_none());

        std::fs::write(home.join(".omp/config.yml"), "memory: [oops\n").expect("broken");
        let err = check(&path, &home).expect_err("refused");
        assert_eq!(err.code, E_OMP_OVERLAY);
        let text = err.to_string();
        assert!(!text.contains("does not know the setting"), "{text}");
        assert!(
            text.contains("error: Settings config is invalid: .omp/config.yml"),
            "{text}"
        );
    }

    #[test]
    fn an_answer_for_another_key_refuses_start() {
        // An omp that answers `config get` with some other setting's JSON does not know ours.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("omp");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = --version ]; then echo 'omp/{CHECKED_OMP_VERSION}'; exit 0; fi\n\
             echo '{{\"key\": \"other\", \"value\": null}}'\n"
        );
        std::fs::write(&path, script).expect("write fake omp");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(
            check(&path, dir.path()).expect_err("refused").code,
            E_OMP_OVERLAY
        );
    }

    #[test]
    fn missing_omp_refuses_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = check(&dir.path().join("no-omp"), dir.path()).expect_err("refused");
        assert_eq!(err.code, E_OMP_OVERLAY);
    }

    #[test]
    fn version_lines_parse() {
        assert_eq!(parse_version("omp/18.22.2\n").as_deref(), Some("18.22.2"));
        assert_eq!(parse_version("v19.0.1 (abc)\n").as_deref(), Some("19.0.1"));
        assert_eq!(parse_version("").as_deref(), None);
        assert_eq!(parse_version("omp/\n").as_deref(), None);
    }
}
