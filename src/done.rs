//! Lane done checks: the Done line a lane starts with, the `lane_done` claim, and the
//! gateway-run checks of it (Dori mode style). Messenger-agnostic.

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::omp;

/// Kill deadline of one `command` signal (Dori mode `COMMAND_TIMEOUT_MS`).
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
/// A check detail is cut to this many characters (Dori mode `short()`).
pub const DETAIL_CHARS: usize = 120;
/// The host tool a lane calls to claim its work is done.
pub const LANE_DONE_TOOL: &str = "lane_done";
/// The first word of an accepted `lane_done` answer (a refusal starts with its code).
pub const ACCEPTED: &str = "ACCEPTED";

/// The Done line cannot be checked.
pub const E_DONE_SYNTAX: &str = "E_DONE_SYNTAX";
/// A `lane_done` claim failed its Done checks.
pub const E_DONE_UNMET: &str = "E_DONE_UNMET";
/// A command exited non-zero or by a signal.
pub const E_CHECK_EXIT: &str = "E_CHECK_EXIT";
/// A file does not exist.
pub const E_CHECK_MISSING: &str = "E_CHECK_MISSING";
/// A command passed `CHECK_TIMEOUT` and was killed.
pub const E_CHECK_TIMEOUT: &str = "E_CHECK_TIMEOUT";
/// A command could not start.
pub const E_CHECK_SPAWN: &str = "E_CHECK_SPAWN";

/// Bytes of command output kept for a failure detail.
const OUTPUT_HEAD_BYTES: u64 = 4096;

/// One checkable part of a Done line.
#[derive(Clone, Debug, PartialEq)]
pub enum Signal {
    /// Runs the argv (no shell); passes on exit code 0.
    Command(Vec<String>),
    /// Passes when the path exists.
    File(String),
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Signal::Command(argv) => write!(f, "command {}", json(argv)),
            Signal::File(path)
                if path.contains(|c: char| c.is_whitespace() || ";\"[]".contains(c)) =>
            {
                write!(f, "file {}", json(path))
            }
            Signal::File(path) => write!(f, "file {path}"),
        }
    }
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

/// A parsed Done line: every signal must pass.
#[derive(Clone, Debug, PartialEq)]
pub struct DoneLine(Vec<Signal>);

impl DoneLine {
    /// Parses a Dori-mode Done line cut to the `command` and `file` kinds.
    pub fn parse(line: &str) -> Result<DoneLine, SyntaxError> {
        let parts = split(strip_prefix(line));
        if parts.is_empty() {
            return Err(SyntaxError {
                part: String::new(),
                reason: SyntaxReason::Empty,
            });
        }
        parts
            .into_iter()
            .map(|part| {
                parse_signal(part).map_err(|reason| SyntaxError {
                    part: part.to_owned(),
                    reason,
                })
            })
            .collect::<Result<_, _>>()
            .map(DoneLine)
    }

    pub fn signals(&self) -> &[Signal] {
        &self.0
    }
}

impl fmt::Display for DoneLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, signal) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{signal}")?;
        }
        Ok(())
    }
}

/// Why a part of a Done line cannot be checked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SyntaxReason {
    Empty,
    Unknown,
    Unsupported,
    BadArgv,
    BadPath,
}

/// A Done line that cannot be checked: the bad part and why.
#[derive(Clone, Debug, PartialEq)]
pub struct SyntaxError {
    pub part: String,
    pub reason: SyntaxReason,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.reason {
            SyntaxReason::Empty => "empty Done line",
            SyntaxReason::Unknown => {
                "unknown signal; use command [\"program\",\"arg\"] or file <path>"
            }
            SyntaxReason::Unsupported => "this signal kind is not supported yet",
            SyntaxReason::BadArgv => {
                "command needs a JSON array of one or more non-empty strings and nothing after it"
            }
            SyntaxReason::BadPath => {
                "file needs one path (quoted if it has spaces) and nothing after it"
            }
        };
        write!(f, "{} -> {reason}", self.part)
    }
}

/// Strips a leading `Done =` (ASCII case-insensitive, spaces optional, `=` required).
fn strip_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    if let Some(head) = trimmed.get(..4)
        && head.eq_ignore_ascii_case("done")
        && let Some(rest) = trimmed[4..].trim_start().strip_prefix('=')
    {
        return rest;
    }
    line
}

/// Splits on `;` at bracket depth 0 outside double-quoted strings; trims parts and
/// drops empty ones.
fn split(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut escaped, mut start) = (0usize, false, false, 0);
    for (index, c) in line.char_indices() {
        if quoted {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                parts.push(&line[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&line[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

fn parse_signal(part: &str) -> Result<Signal, SyntaxReason> {
    let (head, rest) = part
        .split_once(char::is_whitespace)
        .map_or((part, ""), |(head, rest)| (head, rest.trim()));
    match head {
        "command" => match serde_json::from_str::<Vec<String>>(rest) {
            Ok(argv) if !argv.is_empty() && argv.iter().all(|arg| !arg.is_empty()) => {
                Ok(Signal::Command(argv))
            }
            _ => Err(SyntaxReason::BadArgv),
        },
        "file" => {
            let path = if rest.starts_with('"') {
                serde_json::from_str::<String>(rest).map_err(|_| SyntaxReason::BadPath)?
            } else if rest.contains(char::is_whitespace) {
                return Err(SyntaxReason::BadPath);
            } else {
                rest.to_owned()
            };
            if path.is_empty() {
                return Err(SyntaxReason::BadPath);
            }
            Ok(Signal::File(path))
        }
        "merged" | "closed" | "published" | "url" => Err(SyntaxReason::Unsupported),
        _ => Err(SyntaxReason::Unknown),
    }
}

/// The result of one signal; `code == None` means it passed.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub signal: String,
    pub code: Option<&'static str>,
    pub detail: String,
}

pub fn all_passed(checks: &[Check]) -> bool {
    checks.iter().all(|check| check.code.is_none())
}

/// Returned when moraebot started stopping before a check command could start.
#[derive(Debug, PartialEq)]
pub struct Stopped;

/// Runs every signal of `line` in order in `work_dir`. `out_dir` is an existing private folder
/// for the command output files; `env` is the whole environment of a command.
/// `started(pid)` runs right after a command spawned (`false`: stopping, kill it and return
/// `Stopped`); `ended(pid)` runs once per started pid after its leader was reaped and its
/// group killed.
pub(crate) fn run(
    line: &DoneLine,
    work_dir: &Path,
    out_dir: &Path,
    env: &[(OsString, OsString)],
    timeout: Duration,
    mut started: impl FnMut(u32) -> bool,
    mut ended: impl FnMut(u32),
) -> Result<Vec<Check>, Stopped> {
    let mut checks = Vec::with_capacity(line.0.len());
    for signal in &line.0 {
        let (code, detail) = match signal {
            Signal::File(path) => check_file(path, work_dir, env),
            Signal::Command(argv) => check_command(
                argv,
                work_dir,
                out_dir,
                env,
                timeout,
                &mut started,
                &mut ended,
            )?,
        };
        checks.push(Check {
            signal: signal.to_string(),
            code,
            detail: short(&detail),
        });
    }
    Ok(checks)
}

fn check_file(
    path: &str,
    work_dir: &Path,
    env: &[(OsString, OsString)],
) -> (Option<&'static str>, String) {
    let home = env.iter().find(|(name, _)| name == "HOME");
    let resolved: PathBuf = match (path.strip_prefix("~/"), home) {
        (Some(rest), Some((_, home))) => Path::new(home).join(rest),
        _ if Path::new(path).is_absolute() => PathBuf::from(path),
        _ => work_dir.join(path),
    };
    if resolved.exists() {
        (None, "exists".to_owned())
    } else {
        (
            Some(E_CHECK_MISSING),
            format!("missing: {}", resolved.display()),
        )
    }
}

fn check_command(
    argv: &[String],
    work_dir: &Path,
    out_dir: &Path,
    env: &[(OsString, OsString)],
    timeout: Duration,
    started: &mut impl FnMut(u32) -> bool,
    ended: &mut impl FnMut(u32),
) -> Result<(Option<&'static str>, String), Stopped> {
    let (program, args) = argv
        .split_first()
        .expect("DoneLine::parse rejects an empty argv");
    let output = |name: &str| {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(out_dir.join(name))
    };
    let (stdout, stderr) = match (output("stdout"), output("stderr")) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        (Err(error), _) | (_, Err(error)) => return Ok((Some(E_CHECK_SPAWN), error.to_string())),
    };
    // The waiter thread exists before the command, so a thread that cannot start leaves
    // no process behind; it gets the child over a channel and sends back its wait.
    let (child_tx, child_rx) = mpsc::channel::<std::process::Child>();
    let (status_tx, status_rx) = mpsc::channel();
    let waiter = std::thread::Builder::new()
        .name("done-check".to_owned())
        .spawn(move || {
            if let Ok(mut child) = child_rx.recv() {
                let _ = status_tx.send(child.wait());
            }
        });
    if let Err(error) = waiter {
        return Ok((Some(E_CHECK_SPAWN), error.to_string()));
    }
    let spawned = Command::new(program)
        .args(args)
        .current_dir(work_dir)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return Ok((Some(E_CHECK_SPAWN), error.to_string())),
    };
    let pid = child.id();
    if !started(pid) {
        omp::kill_group(pid);
        let _ = child.wait();
        omp::kill_group(pid);
        ended(pid);
        return Err(Stopped);
    }
    let _ = child_tx.send(child);
    let waited = match status_rx.recv_timeout(timeout) {
        Ok(waited) => Some(waited),
        Err(_) => {
            omp::kill_group(pid);
            // SIGKILL ends the leader, so its wait returns; past this bound the waiter
            // thread finishes on its own.
            let _ = status_rx.recv_timeout(omp::ABORT_JOIN_BOUND);
            None
        }
    };
    // A member left in the check's group does not survive the check.
    omp::kill_group(pid);
    ended(pid);
    Ok(match waited {
        None => (
            Some(E_CHECK_TIMEOUT),
            format!("killed after {} s", timeout.as_secs()),
        ),
        Some(Err(error)) => (Some(E_CHECK_EXIT), error.to_string()),
        Some(Ok(status)) => match status.code() {
            Some(0) => (None, "exit 0".to_owned()),
            Some(code) => (
                Some(E_CHECK_EXIT),
                format!("exit {code}: {}", output_head(out_dir)),
            ),
            None => (Some(E_CHECK_EXIT), "exit signal".to_owned()),
        },
    })
}

/// The first bytes of the command's stderr, else of its stdout (lossy UTF-8).
fn output_head(out_dir: &Path) -> String {
    for name in ["stderr", "stdout"] {
        let mut head = Vec::new();
        if let Ok(file) = File::open(out_dir.join(name)) {
            let _ = file.take(OUTPUT_HEAD_BYTES).read_to_end(&mut head);
        }
        if !head.is_empty() {
            return String::from_utf8_lossy(&head).into_owned();
        }
    }
    String::new()
}

/// Folds whitespace runs to one space and cuts to `DETAIL_CHARS` characters.
fn short(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(DETAIL_CHARS)
        .collect()
}

fn check_line(check: &Check) -> String {
    match check.code {
        None => format!("- {} -> ok", check.signal),
        Some(code) => format!("- {} -> {code}: {}", check.signal, check.detail),
    }
}

/// The refusal text of a failed claim, without the code prefix.
pub fn not_done(checks: &[Check]) -> String {
    let mut text = String::from("not done:");
    for check in checks.iter().filter(|check| check.code.is_some()) {
        text.push('\n');
        text.push_str(&check_line(check));
    }
    text.push_str(
        "\nKeep working, then call lane_done again. If you cannot finish, end your turn and say why.",
    );
    text
}

/// The evidence of a claim and every check of it.
pub fn receipt(evidence: &str, checks: &[Check]) -> String {
    let mut text = format!("evidence: {evidence}");
    for check in checks {
        text.push('\n');
        text.push_str(&check_line(check));
    }
    text
}

/// Appended to every lane prompt of a lane with a done line.
pub fn lane_footer(line: &DoneLine) -> String {
    format!(
        "\n\n[moraebot] Done = {line}\nWhen the work is done, call lane_done with your evidence (what you did and how you checked it). moraebot then runs every Done check in the work folder. If one fails, the call is refused with the reasons: keep working and call lane_done again. If you cannot finish, end your turn without calling it and say why; the lead decides."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::time::Duration;

    fn cmd(argv: &[&str]) -> Signal {
        Signal::Command(argv.iter().map(|arg| arg.to_string()).collect())
    }

    fn file(path: &str) -> Signal {
        Signal::File(path.to_owned())
    }

    fn check(signal: &str, code: Option<&'static str>) -> Check {
        Check {
            signal: signal.to_owned(),
            code,
            detail: "d".to_owned(),
        }
    }

    struct Dirs {
        _root: tempfile::TempDir,
        work: PathBuf,
        out: PathBuf,
    }

    fn dirs() -> Dirs {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let out = root.path().join("out");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&out).unwrap();
        Dirs {
            _root: root,
            work,
            out,
        }
    }

    /// Runs `signals` with no env and every start allowed.
    fn run_all(d: &Dirs, signals: Vec<Signal>, timeout: Duration) -> Vec<Check> {
        run(
            &DoneLine(signals),
            &d.work,
            &d.out,
            &[],
            timeout,
            |_| true,
            |_| {},
        )
        .unwrap()
    }

    fn reason(line: &str) -> (String, SyntaxReason) {
        let error = DoneLine::parse(line).unwrap_err();
        (error.part, error.reason)
    }

    fn process_exists(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the pid exists.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    /// Creates the FIFO `alive` in `work` and a reader that reports EOF: every
    /// holder of its write end has exited.
    fn alive_fifo(work: &Path) -> mpsc::Receiver<()> {
        let fifo = work.join("alive");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = File::open(&fifo).unwrap();
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest);
            let _ = tx.send(());
        });
        rx
    }

    #[test]
    fn parse_accepts_command_and_file() {
        let line = DoneLine::parse(r#"command ["cargo","test"]; file target/out.txt"#).unwrap();
        assert_eq!(
            line.signals(),
            [cmd(&["cargo", "test"]), file("target/out.txt")]
        );
        assert_eq!(
            line.to_string(),
            r#"command ["cargo","test"]; file target/out.txt"#
        );
    }

    #[test]
    fn parse_strips_done_prefix_any_case() {
        for text in ["Done = file a", "done=file a", "DONE   =  file a"] {
            assert_eq!(
                DoneLine::parse(text).unwrap().signals(),
                [file("a")],
                "{text}"
            );
        }
    }

    #[test]
    fn parse_splits_only_outside_quotes_and_brackets() {
        let line = DoneLine::parse(r#"command ["sh","-c","a; b"]; file "x;y""#).unwrap();
        assert_eq!(line.signals(), [cmd(&["sh", "-c", "a; b"]), file("x;y")]);
        let Signal::Command(argv) = &line.signals()[0] else {
            panic!("not a command");
        };
        assert_eq!(argv[2], "a; b");
        assert_eq!(line.signals()[1].to_string(), r#"file "x;y""#);
    }

    #[test]
    fn parse_rejects_with_reason() {
        use SyntaxReason::*;
        let cases = [
            ("", "", Empty),
            ("Done = ; ", "", Empty),
            ("wat x", "wat x", Unknown),
            ("merged acme/app#1", "merged acme/app#1", Unsupported),
            ("closed a/b#2", "closed a/b#2", Unsupported),
            ("published p@1", "published p@1", Unsupported),
            (
                "url http://127.0.0.1/",
                "url http://127.0.0.1/",
                Unsupported,
            ),
            ("command []", "command []", BadArgv),
            (r#"command [""]"#, r#"command [""]"#, BadArgv),
            (
                r#"command ["a"] stdout~"x""#,
                r#"command ["a"] stdout~"x""#,
                BadArgv,
            ),
            ("command a b", "command a b", BadArgv),
            ("file", "file", BadPath),
            ("file a b", "file a b", BadPath),
            ("file ok; wat x; file b", "wat x", Unknown),
        ];
        for (line, part, why) in cases {
            assert_eq!(reason(line), (part.to_owned(), why), "{line}");
        }
    }

    #[test]
    fn canonical_line_roundtrips() {
        for text in [
            r#"command ["cargo","test","--locked"]"#,
            r#"file "a[1]"; file b"#,
            r#"Done = file "with space"; command ["sh","-c","x; y"]"#,
            r#"file ~/x.txt; file "q\"uote"; command ["a"]"#,
        ] {
            let line = DoneLine::parse(text).unwrap();
            assert_eq!(DoneLine::parse(&line.to_string()).unwrap(), line, "{text}");
        }
        let brackets = DoneLine::parse(r#"file "a[1]"; file b"#).unwrap();
        assert_eq!(brackets.signals(), [file("a[1]"), file("b")]);
        assert_eq!(brackets.to_string(), r#"file "a[1]"; file b"#);
    }

    #[test]
    fn done_prefix_needs_equals() {
        assert_eq!(reason("Done file a").1, SyntaxReason::Unknown);
    }

    #[test]
    fn not_done_lists_only_failing_checks_with_codes() {
        let text = not_done(&[
            check("file ok", None),
            check("command [\"x\"]", Some(E_CHECK_EXIT)),
            check("file gone", Some(E_CHECK_MISSING)),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "not done:");
        let listed: Vec<&str> = lines[1..]
            .iter()
            .copied()
            .filter(|l| l.starts_with("- "))
            .collect();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].starts_with("- command [\"x\"] -> E_CHECK_EXIT:"));
        assert!(listed[1].starts_with("- file gone -> E_CHECK_MISSING:"));
        assert!(!text.contains("file ok"));
    }

    #[test]
    fn receipt_has_evidence_then_one_line_per_check() {
        let checks = [
            check("file a", None),
            check("file b", Some(E_CHECK_MISSING)),
        ];
        let text = receipt("ran the tests  twice", &checks);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "evidence: ran the tests  twice");
        assert_eq!(lines.len() - 1, checks.len());
        assert!(lines[1].contains("file a") && lines[1].ends_with("ok"));
        assert!(lines[2].contains("file b") && lines[2].contains(E_CHECK_MISSING));
    }

    #[test]
    fn detail_is_folded_and_cut() {
        let d = dirs();
        let script = "i=0; while [ $i -lt 50 ]; do echo 'abcdefghi' >&2; i=$((i+1)); done; exit 1";
        let checks = run_all(&d, vec![cmd(&["/bin/sh", "-c", script])], CHECK_TIMEOUT);
        assert_eq!(std::fs::metadata(d.out.join("stderr")).unwrap().len(), 500);
        assert_eq!(checks[0].code, Some(E_CHECK_EXIT));
        assert!(checks[0].detail.chars().count() <= DETAIL_CHARS);
        assert!(!checks[0].detail.contains('\n'));
    }

    #[test]
    fn file_signal_resolves_relative_absolute_and_home() {
        let d = dirs();
        std::fs::write(d.work.join("here"), "").unwrap();
        std::fs::write(d.out.join("f"), "").unwrap();
        let absolute = d.out.join("f").to_string_lossy().into_owned();
        let env = [(OsString::from("HOME"), d.out.clone().into_os_string())];
        let line = DoneLine(vec![
            file("here"),
            file("gone"),
            file(&absolute),
            file("~/f"),
        ]);
        let checks = run(
            &line,
            &d.work,
            &d.out,
            &env,
            CHECK_TIMEOUT,
            |_| true,
            |_| {},
        )
        .unwrap();
        let codes: Vec<_> = checks.iter().map(|check| check.code).collect();
        assert_eq!(codes, [None, Some(E_CHECK_MISSING), None, None]);
    }

    #[test]
    fn command_exit_codes() {
        let d = dirs();
        let calls = RefCell::new(0);
        let line = DoneLine(vec![
            cmd(&["/bin/sh", "-c", "exit 0"]),
            cmd(&["/bin/sh", "-c", "exit 3"]),
            cmd(&["/no/such/program"]),
        ]);
        let checks = run(
            &line,
            &d.work,
            &d.out,
            &[],
            CHECK_TIMEOUT,
            |_| {
                *calls.borrow_mut() += 1;
                true
            },
            |_| {},
        )
        .unwrap();
        let codes: Vec<_> = checks.iter().map(|check| check.code).collect();
        assert_eq!(codes, [None, Some(E_CHECK_EXIT), Some(E_CHECK_SPAWN)]);
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn command_runs_in_work_dir_with_given_env_only() {
        let d = dirs();
        let argv = cmd(&[
            "/bin/sh",
            "-c",
            "test -f marker && test \"$KEEP\" = 1 && test -z \"$HOME\"",
        ]);
        let env = [(OsString::from("KEEP"), OsString::from("1"))];
        let line = DoneLine(vec![argv]);
        let without = run(
            &line,
            &d.work,
            &d.out,
            &env,
            CHECK_TIMEOUT,
            |_| true,
            |_| {},
        )
        .unwrap();
        assert_eq!(without[0].code, Some(E_CHECK_EXIT));
        std::fs::write(d.work.join("marker"), "").unwrap();
        let with = run(
            &line,
            &d.work,
            &d.out,
            &env,
            CHECK_TIMEOUT,
            |_| true,
            |_| {},
        )
        .unwrap();
        assert_eq!(with[0].code, None);
    }

    #[test]
    fn started_and_ended_pair_per_command() {
        let d = dirs();
        let events = RefCell::new(Vec::new());
        let line = DoneLine(vec![
            cmd(&["/bin/sh", "-c", "exit 0"]),
            cmd(&["/bin/sh", "-c", "exit 1"]),
        ]);
        run(
            &line,
            &d.work,
            &d.out,
            &[],
            CHECK_TIMEOUT,
            |pid| {
                events.borrow_mut().push(("started", pid));
                true
            },
            |pid| events.borrow_mut().push(("ended", pid)),
        )
        .unwrap();
        let events = events.into_inner();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].0, "started");
        assert_eq!(events[1], ("ended", events[0].1));
        assert_eq!(events[2].0, "started");
        assert_eq!(events[3], ("ended", events[2].1));
        assert_ne!(events[0].1, events[2].1);
    }

    #[test]
    fn stop_at_start_kills_and_returns_stopped() {
        let d = dirs();
        let ended = RefCell::new(Vec::new());
        let started = RefCell::new(Vec::new());
        let line = DoneLine(vec![
            cmd(&["/bin/sleep", "600"]),
            cmd(&["/usr/bin/touch", "ran"]),
        ]);
        let result = run(
            &line,
            &d.work,
            &d.out,
            &[],
            CHECK_TIMEOUT,
            |pid| {
                started.borrow_mut().push(pid);
                false
            },
            |pid| ended.borrow_mut().push(pid),
        );
        assert_eq!(result, Err(Stopped));
        let started = started.into_inner();
        assert_eq!(started.len(), 1);
        assert_eq!(ended.into_inner(), started);
        assert!(!process_exists(started[0]));
        assert!(!d.work.join("ran").exists());
    }

    #[test]
    fn timeout_kills_the_command_group() {
        let d = dirs();
        let eof = alive_fifo(&d.work);
        let checks = run_all(
            &d,
            vec![cmd(&[
                "/bin/sh",
                "-c",
                "exec 3>alive; /bin/sleep 600 & wait",
            ])],
            Duration::from_secs(1),
        );
        assert_eq!(checks[0].code, Some(E_CHECK_TIMEOUT));
        eof.recv_timeout(Duration::from_secs(30))
            .expect("every member of the check's group exited");
    }

    #[test]
    fn leftover_group_members_are_killed() {
        let d = dirs();
        let eof = alive_fifo(&d.work);
        let checks = run_all(
            &d,
            vec![cmd(&[
                "/bin/sh",
                "-c",
                "exec 3>alive; /bin/sleep 600 & exit 0",
            ])],
            CHECK_TIMEOUT,
        );
        assert_eq!(checks[0].code, None);
        eof.recv_timeout(Duration::from_secs(30))
            .expect("the final group kill ended the leftover member");
    }
}
