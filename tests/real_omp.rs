//! Opt-in check against the installed omp: the overlay from `omp::ensure_overlay`, given
//! through `PI_CONFIG_FILES`, turns omp's own memory and autolearn off in `--mode rpc`.
//! A control run without the overlay shows every observable can fail. Everything lives
//! in a temp dir (`qa-ohome` as omp's HOME and agent dir); the run only sends
//! `get_state`, never a prompt, and the fake provider at 127.0.0.1:9 is never contacted.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use moraebot::config::{Config, Env};
use moraebot::omp;
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
        .env("FAKE_PROVIDER_KEY", "k111")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(overlay) = overlay {
        command.env(omp::CONFIG_FILES_ENV, overlay);
    }
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
    let tool_names = data["dumpTools"]
        .as_array()
        .unwrap_or_else(|| panic!("dumpTools array: {response}"))
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
        .collect();
    State {
        device_lines,
        tool_names,
    }
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
