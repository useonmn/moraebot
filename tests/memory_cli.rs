use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use moraebot::gateway::STORE_FILE;
use moraebot::memory::history::{self, ACCEPT_REASON};
use moraebot::memory::home::MemoryHome;
use moraebot::memory::{E_MEMORY_FRONTMATTER, WRITER_HUMAN};
use moraebot::store::{E_ALREADY_RUNNING, Store};

const PERSONA: &str = "system/persona.md";
const NOTE: &str = "---\ndescription: a note\n---\nv2-marker\n";

fn moraebot(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_moraebot"));
    cmd.env("MORAEBOT_HOME", home).env("HOME", home);
    cmd
}

fn fake_executable(dir: &Path) -> PathBuf {
    let path = dir.join("omp-stand-in");
    fs::write(&path, "#!/bin/sh\n").expect("write executable");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn run_init(home: &Path, input: &str) -> Output {
    let mut child = moraebot(home)
        .arg("init")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn moraebot");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write answers");
    child.wait_with_output().expect("wait")
}

/// A configured home with the memory layout seeded and imported, and its store closed.
struct Rig {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    memory: MemoryHome,
}

impl Rig {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("qa-mbhome");
        let omp = fake_executable(tmp.path());
        let input = format!("1\n111\n222\nfake-token\n{}\n\n", omp.display());
        let init = run_init(&home, &input);
        assert!(init.status.success(), "init failed");
        let memory = MemoryHome::new(&home);
        memory.ensure_layout().expect("layout");
        let mut store = Store::open(&home.join(STORE_FILE)).expect("store");
        history::prepare(&mut store, &memory).expect("prepare");
        drop(store);
        Self {
            _tmp: tmp,
            home,
            memory,
        }
    }

    fn put(&self, rel: &str, text: &str) {
        let path = self.home.join("memory").join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, text).expect("write file");
    }

    fn accept(&self, extra: &[&str]) -> Output {
        moraebot(&self.home)
            .args(["memory", "accept", "--config"])
            .arg(self.home.join("config.toml"))
            .args(extra)
            .output()
            .expect("run accept")
    }

    fn store(&self) -> Store {
        Store::open(&self.home.join(STORE_FILE)).expect("store")
    }
}

fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout is one JSON line")
}

#[test]
fn accept_records_hand_edit_as_human() {
    let rig = Rig::new();
    let edited = "---\ndescription: edited by hand\n---\nv2-marker\n";
    rig.put(PERSONA, edited);

    let output = rig.accept(&[]);
    assert_eq!(output.status.code(), Some(0));
    let json = stdout_json(&output);
    assert_eq!(json["accepted"], serde_json::json!([PERSONA]));
    let committed = json["committed"].as_i64().expect("numeric commit id");

    let store = rig.store();
    let commits = store.memory_commits().expect("commits");
    let last = commits.last().expect("a commit");
    assert_eq!(last.id, committed);
    assert_eq!(last.commit.writer, WRITER_HUMAN);
    assert_eq!(last.commit.reason, ACCEPT_REASON);
    assert_eq!(
        store.memory_snapshots(last.id).expect("snapshots"),
        vec![(PERSONA.to_string(), Some(edited.to_string()))]
    );
}

#[test]
fn accept_with_nothing_dirty_reports_null() {
    let rig = Rig::new();
    let output = rig.accept(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output),
        serde_json::json!({"committed": null, "accepted": []})
    );
}

#[test]
fn accept_refuses_while_the_store_is_held() {
    let rig = Rig::new();
    let _held = rig.store();
    let output = rig.accept(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with(E_ALREADY_RUNNING));
}

#[test]
fn accept_refuses_bad_frontmatter_and_records_nothing() {
    let rig = Rig::new();
    rig.put(PERSONA, "no frontmatter here\n");
    let before = rig.store().memory_commits().expect("commits").len();

    let output = rig.accept(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with(&format!("{E_MEMORY_FRONTMATTER}:"))
    );
    assert_eq!(rig.store().memory_commits().expect("commits").len(), before);
}

#[test]
fn accept_named_path_and_reason() {
    let rig = Rig::new();
    rig.put("notes/n.md", NOTE);
    rig.put("notes/other.md", NOTE);

    let output = rig.accept(&["--reason", "fix typo", "notes/n.md"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout_json(&output)["accepted"],
        serde_json::json!(["notes/n.md"])
    );

    let store = rig.store();
    let commits = store.memory_commits().expect("commits");
    let last = commits.last().expect("a commit");
    assert_eq!(last.commit.reason, "fix typo");
    assert_eq!(
        store.memory_snapshots(last.id).expect("snapshots"),
        vec![("notes/n.md".to_string(), Some(NOTE.to_string()))]
    );
    let committed = history::committed(&store).expect("committed");
    assert_eq!(
        history::dirty(&rig.memory, &committed).expect("dirty"),
        vec!["notes/other.md".to_string()]
    );
}
