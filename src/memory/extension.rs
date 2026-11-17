//! The lead's omp extension: the shipped TypeScript source, the files it shares with moraebot
//! under `<home>/omp`, and the load check.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::memory::{BEGIN, E_MEMORY_EXT_MISSING, END};
use crate::omp::{private_dir, write_private};

/// The extension file name under `<home>/omp`.
pub const EXT_FILE: &str = "moraebot-memory.ts";
/// The ack the extension writes at session start: `{"pid": <omp pid>, "version": VERSION}`.
pub const ACK_FILE: &str = "moraebot-memory.ack.json";
/// The memory block the extension puts into the system prompt at each prompt.
pub const BLOCK_FILE: &str = "memory-block.md";
/// Entries waiting for hidden delivery, one `.md` file each, delivered in name order.
pub const OUTBOX_DIR: &str = "memory-outbox";
/// The lead-only variable that tells the extension the absolute moraebot home.
pub const HOME_ENV: &str = "MORAEBOT_EXT_HOME";
/// The version the shipped source writes into its ack.
pub const VERSION: &str = "moraebot-memory-1";
/// The `customType` of the hidden message the extension adds.
pub const MESSAGE_TYPE: &str = "moraebot-memory";

/// The shipped extension source.
pub const SOURCE: &str = include_str!("../../assets/omp/moraebot-memory.ts");

/// Sequence of outbox entry names, so entries sort in the order they were queued.
static OUTBOX_SEQ: AtomicU64 = AtomicU64::new(0);

/// `<home>/omp`: the folder of the overlay, the extension and the files they share.
pub fn dir(home: &Path) -> PathBuf {
    home.join("omp")
}

/// Removes `path` (a file, or a folder with its contents); a missing path is fine.
fn remove_any(path: &Path) -> std::io::Result<()> {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Readies a lead start: writes SOURCE as EXT_FILE (only when its bytes differ), removes the
/// ack, empties the outbox (creating it, 0o700), and writes `block` as BLOCK_FILE (removes it
/// for None). Returns the absolute path of EXT_FILE.
pub fn prepare(home: &Path, block: Option<&str>) -> std::io::Result<PathBuf> {
    let dir = dir(home);
    private_dir(&dir)?;
    let ext = dir.join(EXT_FILE);
    if !std::fs::read(&ext).is_ok_and(|bytes| bytes == SOURCE.as_bytes()) {
        write_private(&dir, EXT_FILE, SOURCE.as_bytes())?;
    }
    remove_any(&dir.join(ACK_FILE))?;
    let outbox = dir.join(OUTBOX_DIR);
    remove_any(&outbox)?;
    private_dir(&outbox)?;
    match block {
        Some(block) => write_block(home, block)?,
        None => remove_any(&dir.join(BLOCK_FILE))?,
    }
    std::path::absolute(&ext)
}

/// Replaces BLOCK_FILE with `block` (0o600, temp file and rename).
pub fn write_block(home: &Path, block: &str) -> std::io::Result<()> {
    write_private(&dir(home), BLOCK_FILE, block.as_bytes())
}

/// Adds one outbox entry holding `text` (0o600, temp file and rename); entries sort in the
/// order they were added.
pub fn queue(home: &Path, text: &str) -> std::io::Result<()> {
    let n = OUTBOX_SEQ.fetch_add(1, Ordering::Relaxed);
    write_private(
        &dir(home).join(OUTBOX_DIR),
        &format!("{n:020}.md"),
        text.as_bytes(),
    )
}

/// Ok when the ack names `pid` and VERSION; else Err(E_MEMORY_EXT_MISSING).
pub fn check_ack(home: &Path, pid: Option<u32>) -> Result<(), &'static str> {
    let pid = pid.ok_or(E_MEMORY_EXT_MISSING)?;
    let text = std::fs::read(dir(home).join(ACK_FILE)).map_err(|_| E_MEMORY_EXT_MISSING)?;
    let ack: serde_json::Value = serde_json::from_slice(&text).map_err(|_| E_MEMORY_EXT_MISSING)?;
    let pid_ok = ack
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|acked| acked == u64::from(pid));
    let version_ok = ack.get("version").and_then(serde_json::Value::as_str) == Some(VERSION);
    if pid_ok && version_ok {
        Ok(())
    } else {
        Err(E_MEMORY_EXT_MISSING)
    }
}

/// The memory block at the end of a lead's append text: the tail from the first `BEGIN`, when
/// it ends with `END` and a newline.
pub fn block_of(append: &str) -> Option<&str> {
    let tail = &append[append.find(BEGIN)?..];
    tail.strip_suffix('\n')
        .is_some_and(|body| body.ends_with(END))
        .then_some(tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccessMode, Config, DiscordConfig, OmpConfig, Platform};
    use crate::memory::{self, MEMORY_DIR};
    use crate::omp;
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    fn block() -> String {
        format!("{BEGIN}\nx\n{END}\n")
    }

    fn entries(home: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir(home).join(OUTBOX_DIR))
            .expect("outbox")
            .map(|entry| entry.expect("entry").path())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn source_declares_the_shared_values() {
        let pairs = [
            ("VERSION", VERSION),
            ("HOME_ENV", HOME_ENV),
            ("ACK_FILE", ACK_FILE),
            ("BLOCK_FILE", BLOCK_FILE),
            ("OUTBOX_DIR", OUTBOX_DIR),
            ("MESSAGE_TYPE", MESSAGE_TYPE),
            ("BEGIN", memory::BEGIN),
            ("END", memory::END),
            ("DIRECT_EDIT", memory::E_MEMORY_DIRECT_EDIT),
            ("MEMORY_DIR", MEMORY_DIR),
        ];
        for (name, value) in pairs {
            let line = format!("const {name} = {};", serde_json::Value::from(value));
            assert_eq!(SOURCE.lines().filter(|l| *l == line).count(), 1, "{name}");
        }
        for hook in [
            "\"session_start\"",
            "\"before_agent_start\"",
            "\"tool_call\"",
            "\"tool_result\"",
        ] {
            assert!(SOURCE.contains(hook), "{hook}");
        }
    }

    #[test]
    fn source_imports_only_the_allowed_modules() {
        let imports: BTreeSet<&str> = SOURCE
            .lines()
            .filter(|l| l.starts_with("import "))
            .collect();
        let allowed: BTreeSet<&str> = [
            "import * as fs from \"node:fs\";",
            "import * as path from \"node:path\";",
            "import type { ExtensionAPI } from \"@oh-my-pi/pi-coding-agent\";",
        ]
        .into_iter()
        .collect();
        assert_eq!(imports, allowed);
        for banned in [
            "require(",
            "registerTool",
            "registerProvider",
            concat!("set", "Timeout"),
        ] {
            assert!(!SOURCE.lines().any(|l| l.contains(banned)), "{banned}");
        }
    }

    #[test]
    fn prepare_writes_the_shipped_copy() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let home = tmp.path();
        let b = block();
        let path = prepare(home, Some(&b)).expect("prepare");
        let ext = dir(home).join(EXT_FILE);
        assert_eq!(path, std::path::absolute(&ext).expect("absolute"));
        assert!(path.is_absolute());
        assert_eq!(std::fs::read(&ext).expect("ext"), SOURCE.as_bytes());
        assert_eq!(mode(&ext), 0o600);
        assert_eq!(mode(&dir(home)), 0o700);
        assert_eq!(mode(&dir(home).join(OUTBOX_DIR)), 0o700);
        let block_file = dir(home).join(BLOCK_FILE);
        assert_eq!(std::fs::read_to_string(&block_file).expect("block"), b);
        assert_eq!(mode(&block_file), 0o600);

        std::fs::write(&ext, "x").expect("overwrite");
        prepare(home, Some(&b)).expect("prepare again");
        assert_eq!(std::fs::read(&ext).expect("ext"), SOURCE.as_bytes());

        prepare(home, None).expect("prepare without block");
        assert!(!block_file.exists());
    }

    #[test]
    fn prepare_clears_the_ack_and_the_outbox() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let home = tmp.path();
        prepare(home, None).expect("prepare");
        std::fs::write(dir(home).join(ACK_FILE), "{}").expect("ack");
        queue(home, "a\n").expect("queue a");
        queue(home, "b\n").expect("queue b");
        assert_eq!(entries(home).len(), 2);

        prepare(home, None).expect("prepare again");
        assert!(!dir(home).join(ACK_FILE).exists());
        assert!(dir(home).join(OUTBOX_DIR).is_dir());
        assert!(entries(home).is_empty());
    }

    #[test]
    fn dir_is_the_overlay_folder() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let home = tmp.path().to_path_buf();
        let config = Config {
            messenger: Platform::Discord,
            work_dir: tmp.path().join("work"),
            omp_path: PathBuf::from("omp"),
            home: home.clone(),
            access_mode: AccessMode::OwnerOnly,
            discord: Some(DiscordConfig {
                owner_id: 111,
                channel_id: 222,
                token_file: home.join("secrets").join("discord.token"),
            }),
            slack: None,
            omp: OmpConfig::default(),
        };
        assert_eq!(
            dir(&home),
            omp::overlay_path(&config).parent().expect("parent")
        );
    }

    #[test]
    fn queue_keeps_order_and_modes() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let home = tmp.path();
        prepare(home, None).expect("prepare");
        queue(home, "a\n").expect("queue a");
        queue(home, "b\n").expect("queue b");
        let all = entries(home);
        assert!(all.iter().all(|p| {
            let name = p.file_name().expect("name").to_string_lossy();
            !name.starts_with('.') && name.ends_with(".md")
        }));
        assert_eq!(all.len(), 2);
        let texts: Vec<String> = all
            .iter()
            .map(|p| std::fs::read_to_string(p).expect("entry"))
            .collect();
        assert_eq!(texts, ["a\n", "b\n"]);
        assert!(all.iter().all(|p| mode(p) == 0o600));
    }

    #[test]
    fn write_block_replaces_the_file() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let home = tmp.path();
        prepare(home, None).expect("prepare");
        write_block(home, "one").expect("one");
        write_block(home, "two").expect("two");
        let block_file = dir(home).join(BLOCK_FILE);
        assert_eq!(std::fs::read_to_string(&block_file).expect("block"), "two");
        assert_eq!(mode(&block_file), 0o600);
        let names: BTreeSet<String> = std::fs::read_dir(dir(home))
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        let expected: BTreeSet<String> = [EXT_FILE, OUTBOX_DIR, BLOCK_FILE]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn check_ack_table() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let home = tmp.path();
        std::fs::create_dir_all(dir(home)).expect("dir");
        let ack = dir(home).join(ACK_FILE);
        assert_eq!(check_ack(home, Some(4242)), Err(E_MEMORY_EXT_MISSING));
        let cases = [
            ("not json".to_owned(), Err(E_MEMORY_EXT_MISSING)),
            (
                serde_json::json!({"pid": 4242}).to_string(),
                Err(E_MEMORY_EXT_MISSING),
            ),
            (
                serde_json::json!({"pid": 4243, "version": VERSION}).to_string(),
                Err(E_MEMORY_EXT_MISSING),
            ),
            (
                serde_json::json!({"pid": 4242, "version": "moraebot-memory-0"}).to_string(),
                Err(E_MEMORY_EXT_MISSING),
            ),
            (
                serde_json::json!({"pid": "4242", "version": VERSION}).to_string(),
                Err(E_MEMORY_EXT_MISSING),
            ),
            (
                serde_json::json!({"pid": 4242, "version": VERSION}).to_string(),
                Ok(()),
            ),
        ];
        for (text, expected) in cases {
            std::fs::write(&ack, &text).expect("ack");
            assert_eq!(check_ack(home, Some(4242)), expected, "{text}");
        }
        assert_eq!(check_ack(home, None), Err(E_MEMORY_EXT_MISSING));
    }

    #[test]
    fn block_of_table() {
        let b = block();
        let with_hints = format!("hints\n\n{b}");
        assert_eq!(block_of(&with_hints), Some(b.as_str()));
        assert_eq!(block_of(&b), Some(b.as_str()));
        assert_eq!(block_of("hints"), None);
        assert_eq!(block_of(&format!("{BEGIN}\nx\n")), None);
        assert_eq!(block_of(&format!("{BEGIN}\nx\n{END}")), None);
    }
}
