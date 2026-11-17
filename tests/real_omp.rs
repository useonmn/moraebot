//! Opt-in check against the installed omp: the overlay from `omp::ensure_overlay`, given
//! through `PI_CONFIG_FILES`, turns omp's own memory and autolearn off in `--mode rpc`.
//! A control run without the overlay shows every observable can fail. Everything lives
//! in a temp dir (`qa-ohome` as omp's HOME and agent dir); the run only sends
//! `get_state`, never a prompt, and the fake provider at 127.0.0.1:9 is never contacted.
//!
//! The `real_omp_extension_*` and `real_omp_broken_extension_*` tests drive a real
//! `OmpManager` lead with memory against the installed omp, so the shipped memory
//! extension is written, offered with `-e` and acked as in production. The model is the
//! offline provider extension `tests/fixtures/omp/fake-provider.ts`, pinned by
//! `modelRoles` in a temp agent dir before any prompt (a preflight proves the pin and a
//! control preflight without it proves omp's fallback picks another model); it logs every
//! request to a file and answers by cue words, so no request leaves the process. Run them
//! only in a cleared environment, so no provider key reaches omp:
//! `QA=$(mktemp -d) && env -i PATH="$PATH" HOME="$QA/qa-runhome" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" MORAEBOT_REAL_OMP="$(command -v omp)" cargo test --locked --offline --test real_omp -- --ignored; rm -rf "$QA"`

use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use moraebot::config::{Config, Env};
use moraebot::lead;
use moraebot::memory::extension::{self, OUTBOX_DIR};
use moraebot::memory::home::{MemoryHome, PERSONA_SEED};
use moraebot::memory::{self, history, notice};
use moraebot::omp::{self, OmpManager, Outcome};
use moraebot::store::{Admitted, Store};
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(30);
const DEVICE_TOOLS: [&str; 4] = ["memory_edit", "retain", "recall", "reflect"];

const AGENT_CONFIG: &str = "memory:\n  backend: mnemopi\nmnemopi:\n  noEmbeddings: true\n  llmMode: none\nautolearn:\n  enabled: true\n";
const AGENT_MODELS: &str = "providers:\n  fakeprov:\n    baseUrl: http://127.0.0.1:9/v1\n    api: openai-completions\n    apiKey: FAKE_PROVIDER_KEY\n    models:\n      - id: fake-model\n        name: fake-model\n";

/// `MORAEBOT_REAL_OMP`, else `omp` on `PATH`.
fn real_omp() -> PathBuf {
    if let Some(path) = std::env::var_os("MORAEBOT_REAL_OMP") {
        return PathBuf::from(path);
    }
    let path = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&path)
        .map(|dir| dir.join("omp"))
        .find(|candidate| candidate.is_file())
        .expect("set MORAEBOT_REAL_OMP or put omp on PATH")
}

/// A running omp child; dropping it kills its process group and reaps it.
struct Running {
    child: Child,
    reader: Option<JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: the child leads its own process group (`process_group(0)`).
        unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) };
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// What `get_state` showed: the tool device lines of the system prompt and the tool names.
struct State {
    device_lines: Vec<String>,
    tool_names: Vec<String>,
}

/// Starts omp with `config`'s argv in `session`, sends `get_state` and returns its data.
fn get_state(
    omp_bin: &Path,
    config: &Config,
    session: &Path,
    ohome: &Path,
    overlay: Option<&Path>,
) -> State {
    std::fs::create_dir_all(session).expect("session dir");
    let mut command = Command::new(omp_bin);
    command
        .args(omp::omp_args(config, session, ""))
        .args(["--model", "fakeprov/fake-model"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("PATH is set"))
        .env("HOME", ohome)
        .env("PI_CODING_AGENT_DIR", ohome.join("agent"))
        .env("OMP_SKIP_SETUP", "1")
        .env("FAKE_PROVIDER_KEY", "k111");
    if let Some(overlay) = overlay {
        command.env(omp::CONFIG_FILES_ENV, overlay);
    }
    let response = get_state_response(command);

    let data = &response["data"];
    let prompt: Vec<&str> = data["systemPrompt"]
        .as_array()
        .unwrap_or_else(|| panic!("systemPrompt array: {response}"))
        .iter()
        .map(|part| part.as_str().expect("systemPrompt string"))
        .collect();
    let prompt = prompt.join("\n");
    let device_lines = prompt
        .lines()
        .filter(|line| {
            DEVICE_TOOLS
                .iter()
                .any(|name| line.starts_with(&format!("- {name} — ")))
        })
        .map(str::to_owned)
        .collect();
    State {
        device_lines,
        tool_names: tool_names(&response),
    }
}

/// Spawns `command` in its own process group, sends `get_state` and returns the response
/// frame; the child is killed and reaped before it returns.
fn get_state_response(mut command: Command) -> Value {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().expect("spawn omp");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdin = child.stdin.take().expect("stdin");
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(frame) = serde_json::from_str::<Value>(&line)
                && tx.send(frame).is_err()
            {
                break;
            }
        }
    });
    let running = Running {
        child,
        reader: Some(reader),
    };
    stdin
        .write_all(b"{\"type\":\"get_state\",\"id\":\"s1\"}\n")
        .expect("write get_state");
    stdin.flush().expect("flush");

    let deadline = Instant::now() + WAIT;
    let response = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let frame = rx
            .recv_timeout(left)
            .expect("get_state response within bound");
        if frame["id"] == "s1" {
            break frame;
        }
    };
    drop(stdin);
    drop(running);
    response
}

/// The `dumpTools` names of a `get_state` response.
fn tool_names(response: &Value) -> Vec<String> {
    response["data"]["dumpTools"]
        .as_array()
        .unwrap_or_else(|| panic!("dumpTools array: {response}"))
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
        .collect()
}

fn bank_entries(ohome: &Path) -> usize {
    let banks = ohome.join("agent/memories/mnemopi/banks");
    std::fs::read_dir(banks).map_or(0, |entries| entries.count())
}

#[test]
#[ignore = "needs the installed omp: cargo test --locked --test real_omp -- --ignored"]
fn real_omp_overlay_turns_memory_off() {
    let omp_bin = real_omp();
    let dir = tempfile::tempdir().expect("tempdir");
    let ohome = dir.path().join("qa-ohome");
    std::fs::create_dir_all(ohome.join("agent")).expect("agent dir");
    std::fs::write(ohome.join("agent/config.yml"), AGENT_CONFIG).expect("agent config");
    std::fs::write(ohome.join("agent/models.yml"), AGENT_MODELS).expect("agent models");
    let mbhome = dir.path().join("qa-mbhome");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).expect("work dir");

    let quote = |path: &Path| serde_json::to_string(&path.to_string_lossy()).unwrap();
    let text = format!(
        "owner_id = 111\ntoken_file = {}\nomp_path = {}\nhome = {}\nwork_dir = {}\n[discord]\nchannel_id = 222\n",
        quote(&mbhome.join("secrets/discord.token")),
        quote(&omp_bin),
        quote(&mbhome),
        quote(&work),
    );
    let config = Config::from_toml(&text, &Env::default()).expect("config");
    let sessions = mbhome.join("sessions");

    // Run B: with the production overlay, on the fresh agent dir.
    let overlay = omp::ensure_overlay(&config).expect("overlay");
    let b = get_state(
        &omp_bin,
        &config,
        &sessions.join("b"),
        &ohome,
        Some(&overlay),
    );
    assert!(b.device_lines.is_empty(), "{:?}", b.device_lines);
    for name in ["learn", "manage_skill"] {
        assert!(
            !b.tool_names.iter().any(|n| n == name),
            "{:?}",
            b.tool_names
        );
    }
    assert_eq!(bank_entries(&ohome), 0);

    // Run A: control without the overlay, same agent dir and work dir.
    let a = get_state(&omp_bin, &config, &sessions.join("a"), &ohome, None);
    for name in DEVICE_TOOLS {
        let prefix = format!("- {name} — ");
        assert!(
            a.device_lines.iter().any(|l| l.starts_with(&prefix)),
            "{name}: {:?}",
            a.device_lines
        );
    }
    for name in ["learn", "manage_skill"] {
        assert!(a.tool_names.iter().any(|n| n == name), "{:?}", a.tool_names);
    }
    assert!(bank_entries(&ohome) > 0);
}

const KEY: &str = "discord:th:111";
const BOUND: Duration = Duration::from_secs(60);
const PINNED_MODEL: &str = "fakeprov/fake-model";
const PROVIDER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/omp/fake-provider.ts"
);
const DECOY_MODELS: &str = "providers:\n  decoyprov:\n    baseUrl: http://127.0.0.1:9/v1\n    api: openai-completions\n    apiKey: FAKE_DECOY_KEY_111\n    models:\n      - id: decoy-model\n        name: decoy-model\n";
const PROJECT_NOTE: &str = "---\ndescription: p\n---\nproject notes\n";
const BROKEN_EXTENSION: &str = "export default function () { throw new Error(\"broken-111\"); }\n";

/// The agent config that loads the provider extension; `pinned` adds the `modelRoles` pin.
fn ext_agent_config(pinned: bool) -> String {
    let roles = if pinned {
        "modelRoles:\n  default: fakeprov/fake-model\n"
    } else {
        ""
    };
    format!(
        "extensions:\n  - {}\n{roles}edit:\n  mode: replace\ntools:\n  xdev: false\n",
        serde_json::Value::from(PROVIDER)
    )
}

fn write_agent_dir(ohome: &Path, pinned: bool) {
    std::fs::create_dir_all(ohome.join("agent")).expect("agent dir");
    std::fs::write(ohome.join("agent/config.yml"), ext_agent_config(pinned)).expect("config");
    std::fs::write(ohome.join("agent/models.yml"), DECOY_MODELS).expect("models");
}

/// Starts omp with the lead's argv and no `--model` in a cleared environment with omp's
/// HOME at `ohome`, and returns the selected model (`provider/id`) and the tool names.
fn preflight(
    omp_bin: &Path,
    config: &Config,
    session: &Path,
    ohome: &Path,
) -> (String, Vec<String>) {
    std::fs::create_dir_all(session).expect("session dir");
    let mut command = Command::new(omp_bin);
    command
        .args(omp::omp_args(config, session, ""))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("PATH is set"))
        .env("HOME", ohome)
        .env("PI_CODING_AGENT_DIR", ohome.join("agent"))
        .env("OMP_SKIP_SETUP", "1");
    let response = get_state_response(command);
    let model = &response["data"]["model"];
    let model = format!(
        "{}/{}",
        model["provider"].as_str().unwrap_or_default(),
        model["id"].as_str().unwrap_or_default()
    );
    (model, tool_names(&response))
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("finished within bound")
}

/// The text of one provider message: its string content or its text blocks joined.
fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

fn begins(system: &Value) -> usize {
    system
        .as_str()
        .expect("system string")
        .matches(memory::BEGIN)
        .count()
}

/// The tag line of an owner input for `row`.
fn tag_line(row: i64) -> String {
    lead::owner_input(KEY, row, "")
        .lines()
        .next()
        .expect("tag line")
        .to_owned()
}

fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read session dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            jsonl_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

/// A real lead with memory over the installed omp, all in one temp dir.
struct Rig {
    dir: tempfile::TempDir,
    config: Config,
    store: Arc<Mutex<Store>>,
}

impl Rig {
    /// Checks the environment, writes both agent dirs, runs both preflights, then makes the
    /// moraebot home, the work dir and the store. No `OmpManager` exists yet.
    fn new() -> Self {
        let leaked: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| name.ends_with("_API_KEY") || name.ends_with("_TOKEN"))
            .collect();
        assert!(leaked.is_empty(), "run in a cleared env; found {leaked:?}");

        let omp_bin = real_omp();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let mbhome = root.join("qa-mbhome");
        let work = root.join("work");
        std::fs::create_dir_all(work.join("proj-111")).expect("work dir");
        std::fs::write(work.join("proj-111/a.txt"), "a\n").expect("project file");
        std::fs::create_dir_all(&mbhome).expect("moraebot home");

        let quote = |path: &Path| serde_json::to_string(&path.to_string_lossy()).unwrap();
        let text = format!(
            "owner_id = 111\ntoken_file = {}\nomp_path = {}\nhome = {}\nwork_dir = {}\n[discord]\nchannel_id = 222\n",
            quote(&mbhome.join("secrets/discord.token")),
            quote(&omp_bin),
            quote(&mbhome),
            quote(&work),
        );
        let config = Config::from_toml(&text, &Env::default()).expect("config");

        let ohome = root.join("qa-ohome");
        write_agent_dir(&ohome, true);
        let (model, tools) = preflight(&omp_bin, &config, &root.join("preflight-session"), &ohome);
        assert_eq!(model, PINNED_MODEL);
        for name in ["edit", "ast_edit"] {
            assert!(tools.iter().any(|t| t == name), "{name}: {tools:?}");
        }
        let control = root.join("qa-ohome-control");
        write_agent_dir(&control, false);
        let (model, _) = preflight(
            &omp_bin,
            &config,
            &root.join("preflight-control-session"),
            &control,
        );
        assert_ne!(
            model, PINNED_MODEL,
            "reach: without the pin omp picks another model"
        );

        let store = Store::open(&mbhome.join("inbox.sqlite")).expect("store");
        MemoryHome::new(&mbhome)
            .ensure_layout()
            .expect("memory layout");
        Self {
            dir,
            config,
            store: Arc::new(Mutex::new(store)),
        }
    }

    fn home(&self) -> &Path {
        &self.config.home
    }

    fn memory_file(&self, rel: &str) -> PathBuf {
        self.home().join(memory::MEMORY_DIR).join(rel)
    }

    fn outbox(&self) -> PathBuf {
        extension::dir(self.home()).join(OUTBOX_DIR)
    }

    fn provider_log(&self) -> PathBuf {
        self.dir.path().join("provider.jsonl")
    }

    /// The lead manager with memory; `source` replaces the shipped extension text.
    fn manager(&self, source: Option<&str>) -> OmpManager {
        let ohome = self.dir.path().join("qa-ohome");
        let vars: Vec<(&str, PathBuf)> = vec![
            ("HOME", ohome.clone()),
            ("PI_CODING_AGENT_DIR", ohome.join("agent")),
            ("OMP_SKIP_SETUP", PathBuf::from("1")),
            ("MORAEBOT_TEST_PROVIDER_LOG", self.provider_log()),
            (
                "MORAEBOT_TEST_MEMORY_TARGET",
                self.memory_file("notes/blocked.md"),
            ),
            (
                "MORAEBOT_TEST_EDIT_TARGET",
                self.memory_file("notes/projects/proj-111.md"),
            ),
            ("MORAEBOT_TEST_OUTBOX", self.outbox()),
        ];
        let mut manager =
            OmpManager::new(self.config.clone(), Arc::clone(&self.store)).with_extra_env(vars);
        if let Some(source) = source {
            manager = manager.with_extension_source(source.to_owned());
        }
        manager.with_memory(MemoryHome::new(self.home()))
    }

    /// Writes memory file `rel` by hand and accepts it as an owner commit.
    fn commit(&self, rel: &str, text: &str) {
        let path = self.memory_file(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("memory dir");
        std::fs::write(path, text).expect("write memory file");
        history::accept(
            &mut self.store.lock().unwrap(),
            &MemoryHome::new(self.home()),
            &[],
            "test",
        )
        .expect("accept the hand edit");
    }

    fn change_persona(&self, marker: &str) {
        self.commit(
            memory::PERSONA,
            &format!("---\ndescription: d\n---\n{marker}\n"),
        );
    }

    fn request_refresh(&self) {
        std::fs::write(notice::refresh_file(self.home()), "").expect("refresh request");
    }

    /// Admits and claims an owner row, sends its tagged input and returns the row and reply.
    async fn prompt(&self, omp: &OmpManager, message_id: &str, body: &str) -> (i64, String) {
        let row = {
            let mut store = self.store.lock().unwrap();
            let id = match store
                .admit("discord", message_id, KEY, "222", "111", "body")
                .expect("admit")
            {
                Admitted::New(id) => id,
                Admitted::Duplicate => panic!("duplicate message id"),
            };
            assert!(store.claim(id).expect("claim"));
            id
        };
        let report = bounded(omp.prompt(KEY, row, &lead::owner_input(KEY, row, body))).await;
        match report.main.outcome {
            Outcome::Reply(text) => (row, text),
            other => panic!("reply expected, got code {:?}", other.code()),
        }
    }

    /// Every request the provider extension saw, in order.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.provider_log())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("request json"))
            .collect()
    }

    fn last_request(&self) -> Value {
        self.requests().pop().expect("a provider request")
    }
}

/// The last message of a request.
fn last_message(request: &Value) -> &Value {
    request["messages"]
        .as_array()
        .and_then(|m| m.last())
        .expect("a message")
}

#[tokio::test]
#[ignore = "needs the installed omp; run in a cleared env (file doc): cargo test --locked --offline --test real_omp -- --ignored"]
async fn real_omp_extension_refreshes_the_block_in_place() {
    let rig = Rig::new();
    let omp = rig.manager(None);

    let (_, reply) = rig.prompt(&omp, "m-1", "hello one").await;
    assert_eq!(reply, "ok");
    assert!(!rig.requests().is_empty(), "reach: the provider was chosen");
    assert!(omp.extension_loaded(), "U6: the ack was there in time");
    let pid1 = bounded(omp.child_pid()).await.expect("lead pid");
    let seed_body = PERSONA_SEED.splitn(3, "---\n").nth(2).expect("seed body");
    let last = rig.last_request();
    assert!(
        last["system"]
            .as_str()
            .unwrap()
            .contains(seed_body.trim_end())
    );
    assert_eq!(begins(&last["system"]), 1, "U7");

    rig.change_persona("v2-marker-111");
    rig.request_refresh();
    rig.prompt(&omp, "m-2", "hello two").await;
    assert_eq!(bounded(omp.child_pid()).await, Some(pid1));
    let last = rig.last_request();
    assert!(last["system"].as_str().unwrap().contains("v2-marker-111"));
    assert_eq!(begins(&last["system"]), 1);
    assert!(!notice::refresh_file(rig.home()).exists());

    rig.commit("system/x.md", "---\ndescription: x\n---\nx-111\n");
    let (r3, _) = rig.prompt(&omp, "m-3", "hello three").await;
    let last = rig.last_request();
    let texts: Vec<String> = last["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(text_of)
        .collect();
    let notices: Vec<&String> = texts
        .iter()
        .filter(|t| t.contains(memory::NOTICE_BEGIN))
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("system/x.md"));
    let tag = tag_line(r3);
    let owner = texts
        .iter()
        .find(|t| t.contains(&tag))
        .expect("the owner's message");
    assert!(!owner.contains(memory::NOTICE_BEGIN));
    assert_eq!(bounded(omp.child_pid()).await, Some(pid1));

    let mut files = Vec::new();
    jsonl_files(&omp::lead_session_dir(&rig.config), &mut files);
    let bodies: Vec<String> = files
        .iter()
        .map(|f| std::fs::read_to_string(f).expect("session file"))
        .collect();
    assert!(bodies.iter().all(|b| !b.contains(memory::BEGIN)));
    assert!(
        bodies.iter().any(|b| b.contains(&tag)),
        "reach: the transcript"
    );
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
#[ignore = "needs the installed omp; run in a cleared env (file doc): cargo test --locked --offline --test real_omp -- --ignored"]
async fn real_omp_extension_guards_memory_and_hints_projects() {
    let rig = Rig::new();
    rig.commit("notes/projects/proj-111.md", PROJECT_NOTE);
    let omp = rig.manager(None);
    let project_note = rig.memory_file("notes/projects/proj-111.md");

    for cue in ["cue-write-memory", "cue-edit-memory", "cue-ast-edit-memory"] {
        let (_, reply) = rig.prompt(&omp, cue, cue).await;
        assert_eq!(reply, "done", "{cue}");
        let requests = rig.requests();
        let [.., before, last] = requests.as_slice() else {
            panic!("{cue}: two requests expected");
        };
        assert!(text_of(last_message(before)).contains(cue), "reach: {cue}");
        let result = last_message(last);
        assert_eq!(result["role"], "toolResult", "{cue}");
        assert!(
            text_of(result).contains(memory::E_MEMORY_DIRECT_EDIT),
            "{cue}"
        );
        assert!(!rig.memory_file("notes/blocked.md").exists(), "{cue}");
        assert_eq!(
            std::fs::read_to_string(&project_note).expect("project note"),
            PROJECT_NOTE,
            "{cue}"
        );
    }

    let (_, reply) = rig.prompt(&omp, "m-read", "cue-read-project").await;
    assert_eq!(reply, "done");
    let last = rig.last_request();
    let messages = last["messages"].to_string();
    assert!(messages.contains("notes/projects/proj-111.md"));
    assert!(messages.contains("mid-turn-marker-111"));
    let result = last["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|m| m["role"] == "toolResult")
        .expect("the read result");
    assert_ne!(result["isError"], true);
    let context = last_message(&last);
    assert_eq!(
        context["role"], "developer",
        "reach: the hook's own message"
    );
    let context = text_of(context);
    assert!(context.contains("mid-turn-marker-111"));
    assert!(context.contains("notes/projects/proj-111.md"));
    assert_eq!(std::fs::read_dir(rig.outbox()).expect("outbox").count(), 0);
    bounded(omp.shutdown_all()).await;
}

#[tokio::test]
#[ignore = "needs the installed omp; run in a cleared env (file doc): cargo test --locked --offline --test real_omp -- --ignored"]
async fn real_omp_broken_extension_falls_back_to_restart() {
    let rig = Rig::new();
    let omp = rig.manager(Some(BROKEN_EXTENSION));

    let (_, reply) = rig.prompt(&omp, "m-1", "hello one").await;
    assert_eq!(reply, "ok", "reach: the lead runs with a broken -e module");
    assert!(!omp.extension_loaded());
    let pid1 = bounded(omp.child_pid()).await.expect("lead pid");

    rig.change_persona("v2-marker-111");
    rig.request_refresh();
    rig.prompt(&omp, "m-2", "hello two").await;
    let pid2 = bounded(omp.child_pid()).await.expect("lead pid");
    assert_ne!(pid2, pid1, "restart re-pin");
    let last = rig.last_request();
    assert!(last["system"].as_str().unwrap().contains("v2-marker-111"));
    assert_eq!(begins(&last["system"]), 1);
    assert!(!notice::refresh_file(rig.home()).exists());
    bounded(omp.shutdown_all()).await;
}
