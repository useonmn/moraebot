//! Lane done checks: the Done line a lane starts with, the `lane_done` claim, and the
//! gateway-run checks of it (Dori mode style). Messenger-agnostic.

use std::ffi::{OsStr, OsString};
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
/// A command passed its time limit and was killed, or a URL did not answer within it.
pub const E_CHECK_TIMEOUT: &str = "E_CHECK_TIMEOUT";
/// A command could not start.
pub const E_CHECK_SPAWN: &str = "E_CHECK_SPAWN";
/// The text is not in the file or the command's stdout.
pub const E_CHECK_TEXT: &str = "E_CHECK_TEXT";
/// An existing file or a command's stdout file cannot be read.
pub const E_CHECK_READ: &str = "E_CHECK_READ";
/// The URL answered with a status outside 200-299.
pub const E_CHECK_STATUS: &str = "E_CHECK_STATUS";
/// The URL request failed (connect, TLS, DNS or client setup).
pub const E_CHECK_HTTP: &str = "E_CHECK_HTTP";

/// Bytes of command output kept for a failure detail.
const OUTPUT_HEAD_BYTES: u64 = 4096;
/// Bytes read at a time when a file is searched for a text.
const SCAN_CHUNK: usize = 64 * 1024;

/// One checkable part of a Done line.
#[derive(Clone, Debug, PartialEq)]
pub enum Signal {
    /// Runs the argv (no shell); passes on exit code 0; with a text, only when the whole
    /// stdout also contains it (stderr is never searched).
    Command(Vec<String>, Option<String>),
    /// Passes when the path exists; with a text, only when the file's bytes contain it.
    File(String, Option<String>),
    /// One GET with no redirects and no proxy; passes on a 200-299 status.
    Url(String),
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Signal::Command(argv, text) => {
                write!(f, "command {}", json(argv))?;
                contains(f, text.as_deref())
            }
            Signal::File(path, text) => {
                write!(f, "file {}", word(path))?;
                contains(f, text.as_deref())
            }
            Signal::Url(url) => write!(f, "url {}", word(url)),
        }
    }
}

/// Writes the ` contains "<text>"` tail of a signal that has a text.
fn contains(f: &mut fmt::Formatter<'_>, text: Option<&str>) -> fmt::Result {
    match text {
        Some(text) => write!(f, " contains {}", json(text)),
        None => Ok(()),
    }
}

/// The text JSON-quoted when it has whitespace or one of `;"[]`, else bare.
fn word(text: &str) -> String {
    if text.contains(|c: char| c.is_whitespace() || ";\"[]".contains(c)) {
        json(text)
    } else {
        text.to_owned()
    }
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

/// A parsed Done line: every signal must pass.
#[derive(Clone, Debug, PartialEq)]
pub struct DoneLine(Vec<Signal>);

impl DoneLine {
    /// Parses a Done line; [`Signal`] lists the kinds.
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
    BadText,
    BadUrl,
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
                "unknown signal; use command [\"program\",\"arg\"], file <path> or url <address>"
            }
            SyntaxReason::Unsupported => "this signal kind is not supported yet",
            SyntaxReason::BadArgv => {
                "command needs a JSON array of one or more non-empty strings and nothing after it"
            }
            SyntaxReason::BadPath => {
                "file needs one path (quoted if it has spaces) and nothing after it"
            }
            SyntaxReason::BadText => {
                "contains needs one non-empty quoted text and nothing after it"
            }
            SyntaxReason::BadUrl => {
                "url needs one http or https address with a host, no user or password, and nothing after it"
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
        "command" => match take_json::<Vec<String>>(rest) {
            Some((argv, rest)) if !argv.is_empty() && argv.iter().all(|arg| !arg.is_empty()) => {
                let text = take_contains(rest, SyntaxReason::BadArgv)?;
                Ok(Signal::Command(argv, text))
            }
            _ => Err(SyntaxReason::BadArgv),
        },
        "file" => {
            let (path, rest) = take_word(rest).ok_or(SyntaxReason::BadPath)?;
            let text = take_contains(rest, SyntaxReason::BadPath)?;
            Ok(Signal::File(path, text))
        }
        "url" => {
            let (url, rest) = take_word(rest).ok_or(SyntaxReason::BadUrl)?;
            let parsed = reqwest::Url::parse(&url).map_err(|_| SyntaxReason::BadUrl)?;
            let valid = rest.is_empty()
                && matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some_and(|host| !host.is_empty())
                && parsed.username().is_empty()
                && parsed.password().is_none();
            valid
                .then_some(Signal::Url(url))
                .ok_or(SyntaxReason::BadUrl)
        }
        "merged" | "closed" | "published" => Err(SyntaxReason::Unsupported),
        _ => Err(SyntaxReason::Unknown),
    }
}

/// Reads one leading JSON value; returns it and the rest without leading whitespace.
fn take_json<T: serde::de::DeserializeOwned>(text: &str) -> Option<(T, &str)> {
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<T>();
    let value = stream.next()?.ok()?;
    Some((value, text[stream.byte_offset()..].trim_start()))
}

/// Reads one non-empty word: a JSON string, else the text up to the first whitespace.
fn take_word(text: &str) -> Option<(String, &str)> {
    let (word, rest) = if text.starts_with('"') {
        take_json::<String>(text)?
    } else {
        let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        (word.to_owned(), rest.trim_start())
    };
    (!word.is_empty()).then_some((word, rest))
}

/// Reads an optional `contains "<text>"` tail. Anything else after the kind's own part is
/// `bad`; a `contains` without exactly one non-empty JSON string is `BadText`.
fn take_contains(rest: &str, bad: SyntaxReason) -> Result<Option<String>, SyntaxReason> {
    if rest.is_empty() {
        return Ok(None);
    }
    let (keyword, text) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(keyword, text)| (keyword, text.trim_start()));
    if keyword != "contains" {
        return Err(bad);
    }
    match take_json::<String>(text) {
        Some((text, "")) if !text.is_empty() => Ok(Some(text)),
        _ => Err(SyntaxReason::BadText),
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
            Signal::File(path, text) => check_file(path, text.as_deref(), work_dir, env),
            Signal::Command(argv, text) => {
                let ran = check_command(
                    argv,
                    work_dir,
                    out_dir,
                    env,
                    timeout,
                    &mut started,
                    &mut ended,
                )?;
                match text {
                    Some(text) if ran.0.is_none() => check_text(&out_dir.join("stdout"), text),
                    _ => ran,
                }
            }
            Signal::Url(url) => check_url(url, timeout),
        };
        checks.push(Check {
            signal: signal.to_string(),
            code,
            detail: short(&detail),
        });
    }
    Ok(checks)
}

/// `~/` against the env's `HOME`, an absolute path as is, else relative to `work_dir`.
fn resolve(path: &str, work_dir: &Path, env: &[(OsString, OsString)]) -> PathBuf {
    let home = env.iter().find(|(name, _)| name == "HOME");
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some((_, home))) => Path::new(home).join(rest),
        _ if Path::new(path).is_absolute() => PathBuf::from(path),
        _ => work_dir.join(path),
    }
}

fn check_file(
    path: &str,
    text: Option<&str>,
    work_dir: &Path,
    env: &[(OsString, OsString)],
) -> (Option<&'static str>, String) {
    let resolved = resolve(path, work_dir, env);
    if !resolved.exists() {
        return (
            Some(E_CHECK_MISSING),
            format!("missing: {}", resolved.display()),
        );
    }
    match text {
        None => (None, "exists".to_owned()),
        Some(text) => check_text(&resolved, text),
    }
}

fn check_text(path: &Path, text: &str) -> (Option<&'static str>, String) {
    match file_contains(path, text) {
        Ok(true) => (None, "text found".to_owned()),
        Ok(false) => (Some(E_CHECK_TEXT), "text not found".to_owned()),
        Err(error) => (Some(E_CHECK_READ), error.kind().to_string()),
    }
}

/// Searches the whole file for the bytes of `text` (non-empty), `SCAN_CHUNK` bytes at a
/// time; the last `text.len() - 1` bytes of a chunk are kept in front of the next one so a
/// text across a chunk boundary is found.
fn file_contains(path: &Path, text: &str) -> std::io::Result<bool> {
    let needle = text.as_bytes();
    let keep = needle.len() - 1;
    let mut file = File::open(path)?;
    let mut chunk = vec![0; SCAN_CHUNK];
    let mut buffer = Vec::with_capacity(keep + SCAN_CHUNK);
    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => return Ok(false),
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(needle.len()).any(|window| window == needle) {
            return Ok(true);
        }
        buffer.drain(..buffer.len().saturating_sub(keep));
    }
}

/// Sends one GET to `url` with no redirects, no proxy and `timeout` for the whole request;
/// the body is not read.
fn check_url(url: &str, timeout: Duration) -> (Option<&'static str>, String) {
    match http_call(
        timeout,
        |client| async move { client.get(url).send().await },
    ) {
        Err(detail) => (Some(E_CHECK_HTTP), detail),
        Ok(Err(error)) => request_failed(error, timeout),
        Ok(Ok(response)) => {
            let status = response.status();
            let code = (!status.is_success()).then_some(E_CHECK_STATUS);
            (code, format!("status {}", status.as_u16()))
        }
    }
}

/// Runs `call` with a client that follows no redirects, uses no proxy and gives up after
/// `timeout`; `Err` holds the detail when the runtime or the client cannot be built.
fn http_call<T, F: std::future::Future<Output = T>>(
    timeout: Duration,
    call: impl FnOnce(reqwest::Client) -> F,
) -> Result<T, String> {
    // The `lane_done` handler runs on a plain thread with no runtime, so a private one is safe.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    // The client's timer and connector need the runtime's context already when it is built.
    let _context = runtime.enter();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(timeout)
        .build()
        .map_err(|error| error.without_url().to_string())?;
    Ok(runtime.block_on(call(client)))
}

/// Maps a request that got no answer to `E_CHECK_TIMEOUT` or `E_CHECK_HTTP`.
fn request_failed(error: reqwest::Error, timeout: Duration) -> (Option<&'static str>, String) {
    if error.is_timeout() {
        (
            Some(E_CHECK_TIMEOUT),
            format!("no answer after {} s", timeout.as_secs()),
        )
    } else {
        (Some(E_CHECK_HTTP), error.without_url().to_string())
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
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let opts = RunOpts {
        out_dir,
        env,
        timeout,
    };
    Ok(
        match run_command(program.as_ref(), &args, work_dir, &opts, started, ended)? {
            Ran::Spawn(detail) => (Some(E_CHECK_SPAWN), detail),
            Ran::Exited(0) => (None, "exit 0".to_owned()),
            Ran::Exited(code) => (
                Some(E_CHECK_EXIT),
                format!("exit {code}: {}", output_head(out_dir)),
            ),
            Ran::Signalled => (Some(E_CHECK_EXIT), "exit signal".to_owned()),
            Ran::WaitError(error) => (Some(E_CHECK_EXIT), error),
            Ran::Timeout => (
                Some(E_CHECK_TIMEOUT),
                format!("killed after {} s", timeout.as_secs()),
            ),
        },
    )
}

/// How a command run by [`run_command`] ended.
pub(crate) enum Ran {
    Spawn(String),
    Exited(i32),
    Signalled,
    WaitError(String),
    Timeout,
}

/// Where a command run by [`run_command`] writes its output, its environment and its kill deadline.
pub(crate) struct RunOpts<'a> {
    pub(crate) out_dir: &'a Path,
    pub(crate) env: &'a [(OsString, OsString)],
    pub(crate) timeout: Duration,
}

/// Runs `program` with `args` in `dir` as the leader of its own process group, with exactly
/// `opts.env`, stdin null and stdout/stderr in `opts.out_dir`. `started(pid)` runs right after
/// the spawn (`false`: stopping, kill it and return `Stopped`); `ended(pid)` runs once per
/// started pid after its leader was reaped and its group killed.
pub(crate) fn run_command(
    program: &OsStr,
    args: &[OsString],
    dir: &Path,
    opts: &RunOpts<'_>,
    started: &mut impl FnMut(u32) -> bool,
    ended: &mut impl FnMut(u32),
) -> Result<Ran, Stopped> {
    let output = |name: &str| {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(opts.out_dir.join(name))
    };
    let (stdout, stderr) = match (output("stdout"), output("stderr")) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        (Err(error), _) | (_, Err(error)) => return Ok(Ran::Spawn(error.to_string())),
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
        return Ok(Ran::Spawn(error.to_string()));
    }
    let spawned = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(opts.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return Ok(Ran::Spawn(error.to_string())),
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
    let waited = match status_rx.recv_timeout(opts.timeout) {
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
        None => Ran::Timeout,
        Some(Err(error)) => Ran::WaitError(error.to_string()),
        Some(Ok(status)) => status.code().map_or(Ran::Signalled, Ran::Exited),
    })
}

/// The first bytes of the command's stderr, else of its stdout (lossy UTF-8).
pub(crate) fn output_head(out_dir: &Path) -> String {
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
pub(crate) fn short(text: &str) -> String {
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::os::unix::ffi::OsStrExt;
    use std::time::Duration;

    fn cmd(argv: &[&str]) -> Signal {
        Signal::Command(argv.iter().map(|arg| arg.to_string()).collect(), None)
    }

    fn cmd_text(argv: &[&str], text: &str) -> Signal {
        Signal::Command(
            argv.iter().map(|arg| arg.to_string()).collect(),
            Some(text.to_owned()),
        )
    }

    fn file(path: &str) -> Signal {
        Signal::File(path.to_owned(), None)
    }

    fn file_text(path: &str, text: &str) -> Signal {
        Signal::File(path.to_owned(), Some(text.to_owned()))
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

    /// Serves exactly `statuses.len()` connections in order on 127.0.0.1: sends each
    /// request's first line on the channel and answers with the next status.
    fn http_server(statuses: &[u16]) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let statuses = statuses.to_vec();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for status in statuses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new((&stream).take(8192));
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                    line.clear();
                }
                let _ = tx.send(first.trim_end().to_owned());
                let _ = (&stream).write_all(
                    format!(
                        "HTTP/1.1 {status} QA\r\nLocation: /moved\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
            }
        });
        (base, rx)
    }

    /// Accepts one connection on 127.0.0.1, reads the request and never answers; closes
    /// when the returned sender is used or dropped.
    fn silent_server() -> (String, mpsc::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (release, wait) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new((&stream).take(8192));
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                line.clear();
            }
            let _ = wait.recv();
        });
        (base, release)
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
        let Signal::Command(argv, _) = &line.signals()[0] else {
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
            ("command []", "command []", BadArgv),
            (r#"command [""]"#, r#"command [""]"#, BadArgv),
            (
                r#"command ["a"] stdout~"x""#,
                r#"command ["a"] stdout~"x""#,
                BadArgv,
            ),
            ("command a b", "command a b", BadArgv),
            (
                r#"command ["a"] contains"#,
                r#"command ["a"] contains"#,
                BadText,
            ),
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
            r#"file report.txt contains "0 failed""#,
            r#"file "a b" contains "q\"uote;x""#,
            r#"command ["sh","-c","x"] contains "a;b""#,
            r#"url http://127.0.0.1:8080/health; file a contains "b""#,
        ] {
            let line = DoneLine::parse(text).unwrap();
            assert_eq!(DoneLine::parse(&line.to_string()).unwrap(), line, "{text}");
        }
        let brackets = DoneLine::parse(r#"file "a[1]"; file b"#).unwrap();
        assert_eq!(brackets.signals(), [file("a[1]"), file("b")]);
        assert_eq!(brackets.to_string(), r#"file "a[1]"; file b"#);
    }

    #[test]
    fn parse_accepts_file_contains() {
        let line = DoneLine::parse(r#"file report.txt contains "0 failed""#).unwrap();
        assert_eq!(line.signals(), [file_text("report.txt", "0 failed")]);
        assert_eq!(line.to_string(), r#"file report.txt contains "0 failed""#);
        let line = DoneLine::parse(r#"file "a b" contains "x;y"; file c"#).unwrap();
        assert_eq!(line.signals(), [file_text("a b", "x;y"), file("c")]);
        assert_eq!(line.to_string(), r#"file "a b" contains "x;y"; file c"#);
    }

    #[test]
    fn parse_accepts_command_contains() {
        let text = r#"command ["cargo","test"] contains "0 failed""#;
        let line = DoneLine::parse(text).unwrap();
        assert_eq!(line.signals(), [cmd_text(&["cargo", "test"], "0 failed")]);
        assert_eq!(line.to_string(), text);
    }

    #[test]
    fn command_contains_reads_stdout_only() {
        let d = dirs();
        let checks = run_all(
            &d,
            vec![
                cmd_text(&["/bin/sh", "-c", "echo all-ok"], "all-ok"),
                cmd_text(&["/bin/sh", "-c", "echo nope"], "all-ok"),
                cmd_text(&["/bin/sh", "-c", "echo all-ok >&2"], "all-ok"),
                cmd_text(&["/bin/sh", "-c", "echo all-ok; exit 1"], "all-ok"),
                cmd_text(
                    &[
                        "/bin/sh",
                        "-c",
                        "head -c 70000 /dev/zero | tr '\\0' a; echo all-ok",
                    ],
                    "all-ok",
                ),
            ],
            CHECK_TIMEOUT,
        );
        let codes: Vec<_> = checks.iter().map(|check| check.code).collect();
        assert_eq!(
            codes,
            [
                None,
                Some(E_CHECK_TEXT),
                Some(E_CHECK_TEXT),
                Some(E_CHECK_EXIT),
                None
            ]
        );
    }

    #[test]
    fn parse_rejects_bad_contains() {
        use SyntaxReason::*;
        let cases = [
            ("file a contains", BadText),
            (r#"file a contains """#, BadText),
            ("file a contains x", BadText),
            (r#"file a contains "x" y"#, BadText),
            (r#"file a contain "x""#, BadPath),
        ];
        for (line, why) in cases {
            assert_eq!(reason(line), (line.to_owned(), why), "{line}");
        }
    }

    #[test]
    fn file_contains_checks_text() {
        let d = dirs();
        std::fs::write(d.work.join("report.txt"), "tests: 0 failed\n").unwrap();
        std::fs::create_dir(d.work.join("qa-dir")).unwrap();
        let checks = run_all(
            &d,
            vec![
                file_text("report.txt", "0 failed"),
                file_text("report.txt", "1 failed"),
                file_text("gone", "x"),
                file_text("qa-dir", "x"),
                file("report.txt"),
            ],
            CHECK_TIMEOUT,
        );
        let codes: Vec<_> = checks.iter().map(|check| check.code).collect();
        assert_eq!(
            codes,
            [
                None,
                Some(E_CHECK_TEXT),
                Some(E_CHECK_MISSING),
                Some(E_CHECK_READ),
                None
            ]
        );
    }

    #[test]
    fn text_found_across_a_read_boundary() {
        let d = dirs();
        let mut big = vec![b'a'; SCAN_CHUNK - 2];
        let mut tail = big.clone();
        big.extend_from_slice(b"NEEDLE");
        tail.extend_from_slice(b"NEED");
        std::fs::write(d.work.join("qa-big.txt"), big).unwrap();
        std::fs::write(d.work.join("qa-tail.txt"), tail).unwrap();
        let checks = run_all(
            &d,
            vec![
                file_text("qa-big.txt", "NEEDLE"),
                file_text("qa-tail.txt", "NEEDLE"),
            ],
            CHECK_TIMEOUT,
        );
        let codes: Vec<_> = checks.iter().map(|check| check.code).collect();
        assert_eq!(codes, [None, Some(E_CHECK_TEXT)]);
    }

    #[test]
    fn parse_accepts_url_kinds() {
        let line = DoneLine::parse("url http://127.0.0.1:8080/health").unwrap();
        assert_eq!(
            line.signals(),
            [Signal::Url("http://127.0.0.1:8080/health".to_owned())]
        );
        assert_eq!(line.to_string(), "url http://127.0.0.1:8080/health");
        let line = DoneLine::parse(r#"url "https://example.invalid/a;b""#).unwrap();
        assert_eq!(
            line.signals(),
            [Signal::Url("https://example.invalid/a;b".to_owned())]
        );
        assert_eq!(line.to_string(), r#"url "https://example.invalid/a;b""#);
    }

    #[test]
    fn parse_rejects_bad_urls() {
        for line in [
            "url",
            "url ftp://127.0.0.1/",
            "url http://",
            "url not-a-url",
            "url http://qa:pw@127.0.0.1/",
            "url http://127.0.0.1/ x",
            r#"url http://127.0.0.1/ contains "x""#,
        ] {
            assert_eq!(
                reason(line),
                (line.to_owned(), SyntaxReason::BadUrl),
                "{line}"
            );
        }
    }

    #[test]
    fn url_passes_on_2xx_only() {
        let d = dirs();
        let (base, rx) = http_server(&[200, 204, 302, 404, 500]);
        let signals = (0..5)
            .map(|_| Signal::Url(format!("{base}/health")))
            .collect();
        let checks = run_all(&d, signals, CHECK_TIMEOUT);
        let codes: Vec<_> = checks.iter().map(|check| check.code).collect();
        assert_eq!(
            codes,
            [
                None,
                None,
                Some(E_CHECK_STATUS),
                Some(E_CHECK_STATUS),
                Some(E_CHECK_STATUS)
            ]
        );
        let details: Vec<_> = checks[2..]
            .iter()
            .map(|check| check.detail.as_str())
            .collect();
        assert_eq!(details, ["status 302", "status 404", "status 500"]);
        for _ in 0..5 {
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(30)).unwrap(),
                "GET /health HTTP/1.1"
            );
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn url_unreachable_is_an_http_failure() {
        let d = dirs();
        let checks = run_all(
            &d,
            vec![Signal::Url("http://127.0.0.1:0/health".to_owned())],
            CHECK_TIMEOUT,
        );
        assert_eq!(checks[0].code, Some(E_CHECK_HTTP));
    }

    /// Time-limit exception: the 1 s timeout is the behavior under test.
    #[test]
    fn url_timeout_gives_check_timeout() {
        let d = dirs();
        let (base, release) = silent_server();
        let checks = run_all(
            &d,
            vec![Signal::Url(format!("{base}/health"))],
            Duration::from_secs(1),
        );
        assert_eq!(checks[0].code, Some(E_CHECK_TIMEOUT));
        drop(release);
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
