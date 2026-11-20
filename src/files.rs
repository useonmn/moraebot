//! Received and outbound files: limits, names, paths, downloads. Messenger-agnostic.

use std::fmt;
use std::fs::{DirBuilder, File};
use std::future::Future;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::adapter::AdapterError;

/// Most attachments accepted on one inbound message.
pub const MAX_FILES: usize = 10;
/// Folder under the work folder that holds received files.
pub const INBOX_DIR: &str = ".moraebot-inbox";
/// Longest gap without a received byte before a download fails.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest one outbound file's upload call may take; past it the file is skipped, never retried.
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Most upload attempts per outbound file before it is skipped.
pub const MAX_UPLOAD_ATTEMPTS: u32 = 3;

/// The message carries more attachments than `MAX_FILES`.
pub const E_FILE_TOO_MANY: &str = "E_FILE_TOO_MANY";
/// A download failed or stalled.
pub const E_FILE_FETCH: &str = "E_FILE_FETCH";
/// A download URL points at a host that is not allowed.
pub const E_FILE_HOST: &str = "E_FILE_HOST";
/// The messenger reported the file as unavailable.
pub const E_FILE_REMOTE: &str = "E_FILE_REMOTE";
/// A received file could not be written to disk.
pub const E_FILE_WRITE: &str = "E_FILE_WRITE";
/// An outbound path is outside the work folder, inside the moraebot home, or not a regular file.
pub const E_FILE_PATH: &str = "E_FILE_PATH";
/// Not every attachment of a message could be received.
pub const E_FILE_INCOMPLETE: &str = "E_FILE_INCOMPLETE";
/// The messenger refused access to the file (missing permission or scope).
pub const E_FILE_DENIED: &str = "E_FILE_DENIED";
/// An upload call ran past `UPLOAD_TIMEOUT`.
pub const E_FILE_UPLOAD_TIMEOUT: &str = "E_FILE_UPLOAD_TIMEOUT";
/// A download streamed more bytes than its [`Progress`] limit allows.
pub const E_FILE_TOO_LARGE: &str = "E_FILE_TOO_LARGE";

/// Bytes received so far and when the last chunk arrived, for the stall watchdog.
pub struct Progress {
    bytes: AtomicU64,
    last: Mutex<tokio::time::Instant>,
    limit: Option<u64>,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            bytes: AtomicU64::new(0),
            last: Mutex::new(tokio::time::Instant::now()),
            limit: None,
        }
    }

    /// Like [`Self::new`], but the download must stop before it writes more than `limit`
    /// bytes (see [`Self::check_limit`]).
    pub fn with_limit(limit: u64) -> Self {
        Self {
            limit: Some(limit),
            ..Self::new()
        }
    }

    /// `E_FILE_TOO_LARGE` when `total` bytes would pass the limit. Adapters call this with
    /// the running total before writing each chunk.
    pub fn check_limit(&self, total: u64) -> Result<(), AdapterError> {
        match self.limit {
            Some(limit) if total > limit => Err(error(
                E_FILE_TOO_LARGE,
                "download is larger than its size limit",
            )),
            _ => Ok(()),
        }
    }

    /// The byte limit set by [`Self::with_limit`]; `None` means no limit.
    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// Records a received chunk of `n` bytes at the current instant.
    pub fn record(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = tokio::time::Instant::now();
    }

    /// When the last chunk arrived (or when tracking started).
    pub fn last(&self) -> tokio::time::Instant {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Total bytes recorded.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

/// A received file as stored in the inbox row and listed in the prompt.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFile {
    pub path: PathBuf,
    pub name: String,
    pub mime: Option<String>,
    pub bytes: u64,
}

/// Never prints the path or the name.
impl fmt::Debug for SavedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavedFile")
            .field("mime", &self.mime)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// A file the agent asked to send, with its delivery state.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutFile {
    pub path: PathBuf,
    pub name: String,
    pub sent: bool,
    pub skipped: bool,
    /// An upload of this file may be in flight.
    #[serde(default)]
    pub attempting: bool,
    /// Upload attempts so far, compared with `MAX_UPLOAD_ATTEMPTS`.
    #[serde(default)]
    pub attempts: u32,
}

/// Never prints the path or the name.
impl fmt::Debug for OutFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutFile")
            .field("sent", &self.sent)
            .field("skipped", &self.skipped)
            .field("attempting", &self.attempting)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

fn error(code: &'static str, message: &str) -> AdapterError {
    AdapterError {
        code,
        message: message.to_owned(),
    }
}

/// Longest sanitized name, in bytes, before the index prefix.
const MAX_NAME_BYTES: usize = 100;

/// Safe on-disk name for attachment `index`: last path component only, characters outside
/// `[A-Za-z0-9._-]` become `_`, leading dots are stripped, at most 100 bytes, empty becomes
/// `file`, and the result is prefixed `<index>-`.
pub fn sanitize_name(raw: &str, index: usize) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let mapped: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut name = mapped.trim_start_matches('.').to_owned();
    let mut cut = name.len().min(MAX_NAME_BYTES);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    name.truncate(cut);
    if name.is_empty() {
        name.push_str("file");
    }
    format!("{index}-{name}")
}

/// `<work_dir>/.moraebot-inbox/<key_slug>/<row_id>`.
pub fn inbox_dir(work_dir: &Path, key_slug: &str, row_id: i64) -> PathBuf {
    work_dir
        .join(INBOX_DIR)
        .join(key_slug)
        .join(row_id.to_string())
}

/// Creates `path` and every missing parent with mode 0700; existing levels are untouched.
pub fn create_private_dir_all(path: &Path) -> Result<(), AdapterError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| write_error(&e))
}

/// Parses `url` and accepts it only as https on one of `allowed_hosts` (exact match) with no
/// user info.
pub fn check_url(url: &str, allowed_hosts: &[&str]) -> Result<reqwest::Url, AdapterError> {
    let refused = || error(E_FILE_HOST, "download URL is not allowed");
    let parsed = reqwest::Url::parse(url).map_err(|_| refused())?;
    let host_ok = parsed
        .host_str()
        .is_some_and(|h| allowed_hosts.contains(&h));
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !host_ok
    {
        return Err(refused());
    }
    Ok(parsed)
}

/// Maps a write failure (disk full included) to `E_FILE_WRITE`; the message names the error
/// kind only, never a path.
pub fn write_error(err: &io::Error) -> AdapterError {
    AdapterError {
        code: E_FILE_WRITE,
        message: format!("cannot write file ({:?})", err.kind()),
    }
}

/// Streams `url` into `dest` through `<dest>.part` (0600, created new), recording every
/// written chunk in `progress`, and renames it to `dest` on success. Fails with
/// `E_FILE_TOO_LARGE` (the `.part` file removed) before writing past `progress`'s limit.
/// No timer: the caller wraps this in `with_stall_timeout`. The client must not follow
/// redirects; a 3xx is `E_FILE_FETCH` anyway.
pub async fn fetch_to_file(
    client: &reqwest::Client,
    url: reqwest::Url,
    bearer: Option<&str>,
    dest: &Path,
    reject_html: bool,
    progress: &Progress,
) -> Result<u64, AdapterError> {
    if tokio::fs::symlink_metadata(dest).await.is_ok() {
        return Err(write_error(&io::Error::from(io::ErrorKind::AlreadyExists)));
    }
    let mut request = client.get(url);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let fetch_failed = || error(E_FILE_FETCH, "download failed");
    let mut response = request.send().await.map_err(|_| fetch_failed())?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(error(E_FILE_DENIED, "download refused"));
    }
    if !status.is_success() {
        return Err(fetch_failed());
    }
    if reject_html {
        let html = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"));
        if html {
            return Err(error(
                E_FILE_DENIED,
                "download returned a web page instead of the file",
            ));
        }
    }

    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&part)
        .await
        .map_err(|e| write_error(&e))?;
    let streamed = async {
        let mut total = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|_| fetch_failed())? {
            progress.check_limit(total + chunk.len() as u64)?;
            file.write_all(&chunk).await.map_err(|e| write_error(&e))?;
            progress.record(chunk.len() as u64);
            total += chunk.len() as u64;
        }
        file.flush().await.map_err(|e| write_error(&e))?;
        drop(file);
        tokio::fs::rename(&part, dest)
            .await
            .map_err(|e| write_error(&e))?;
        Ok(total)
    }
    .await;
    if streamed.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    streamed
}

/// The stall watchdog around every adapter fetch: fails with `E_FILE_FETCH` once
/// `STALL_TIMEOUT` passes after the last recorded chunk (or after `progress` was created),
/// with no total time limit. On a stall `fetch` is dropped, which cancels it.
pub async fn with_stall_timeout<T>(
    fetch: impl Future<Output = Result<T, AdapterError>>,
    progress: &Progress,
) -> Result<T, AdapterError> {
    let mut fetch = std::pin::pin!(fetch);
    loop {
        tokio::select! {
            biased;
            result = &mut fetch => return result,
            _ = tokio::time::sleep_until(progress.last() + STALL_TIMEOUT) => {
                if tokio::time::Instant::now() >= progress.last() + STALL_TIMEOUT {
                    return Err(error(E_FILE_FETCH, "download stalled"));
                }
            }
        }
    }
}

/// The one HTTP client for file transfers: no redirects and a 10 s connect timeout, nothing
/// else (no read or total timeout: the stall watchdog bounds downloads).
pub fn file_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("the TLS backend initializes")
}

/// One POST with `body` as the whole request body and no `Authorization` header. Returns
/// the HTTP status, or `None` on any network error. The caller host-checks `url`.
pub async fn post_bytes(client: &reqwest::Client, url: reqwest::Url, body: Vec<u8>) -> Option<u16> {
    let response = client.post(url).body(body).send().await.ok()?;
    Some(response.status().as_u16())
}

/// Checks a path the agent wants to send: a relative `path` is joined onto `work_dir`; the
/// canonical result must be a regular file inside `work_dir`, outside `home`, and openable.
/// Returns the canonical path and its size. Anything that is not a regular file is refused
/// before any open, so a named pipe never blocks the caller.
pub fn check_outbound(
    path: &Path,
    work_dir: &Path,
    home: &Path,
) -> Result<(PathBuf, u64), AdapterError> {
    let refused = |why: &str| error(E_FILE_PATH, why);
    let joined = if path.is_relative() {
        work_dir.join(path)
    } else {
        path.to_owned()
    };
    let canonical = joined
        .canonicalize()
        .map_err(|_| refused("file not found"))?;
    let work = work_dir
        .canonicalize()
        .map_err(|_| refused("work folder not found"))?;
    if !canonical.starts_with(&work) {
        return Err(refused("file is outside the work folder"));
    }
    if let Ok(home) = home.canonicalize()
        && canonical.starts_with(&home)
    {
        return Err(refused("file is inside the moraebot home"));
    }
    let meta = std::fs::metadata(&canonical).map_err(|_| refused("file not found"))?;
    if !meta.is_file() {
        return Err(refused("not a regular file"));
    }
    let file = File::open(&canonical).map_err(|_| refused("file cannot be read"))?;
    let meta = file
        .metadata()
        .map_err(|_| refused("file cannot be read"))?;
    if !meta.is_file() {
        return Err(refused("not a regular file"));
    }
    Ok((canonical, meta.len()))
}

/// Header line of the attached-files block appended to the prompt.
const FILES_HEADER: &str = "[Attached files, saved on disk]";

fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect()
}

/// The prompt text for a message with received files: `text` unchanged when there are no
/// files; otherwise the text, a blank line and the attached-files block, or the block alone
/// when the text is blank.
pub fn prompt_with_files(text: &str, files: &[SavedFile]) -> String {
    if files.is_empty() {
        return text.to_owned();
    }
    let lines: Vec<String> = files
        .iter()
        .map(|f| {
            format!(
                "- {} (name: {}, type: {}, {} bytes)",
                f.path.display(),
                one_line(&f.name),
                one_line(f.mime.as_deref().unwrap_or("unknown")),
                f.bytes
            )
        })
        .collect();
    let block = format!("{FILES_HEADER}\n{}", lines.join("\n"));
    if text.trim().is_empty() {
        block
    } else {
        format!("{text}\n\n{block}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    const WAIT: Duration = Duration::from_secs(30);

    async fn listen() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        (listener, base)
    }

    /// Reads the request head up to the blank line.
    async fn read_head(stream: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            head.push(byte[0]);
        }
        String::from_utf8(head).unwrap()
    }

    fn content_length(head: &str) -> usize {
        head.lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().unwrap())
            })
            .unwrap_or(0)
    }

    /// Serves one connection: reads the head, writes each piece in turn, then closes.
    fn serve_once(listener: TcpListener, pieces: Vec<Vec<u8>>) -> tokio::task::JoinHandle<String> {
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let head = read_head(&mut stream).await;
            for piece in pieces {
                if stream.write_all(&piece).await.is_err() {
                    break;
                }
                let _ = stream.flush().await;
            }
            head
        })
    }

    fn chunk(data: &[u8]) -> Vec<u8> {
        let mut out = format!("{:x}\r\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
        out
    }

    const CHUNKED_HEAD: &[u8] =
        b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ntransfer-encoding: chunked\r\n\r\n";

    fn url(base: &str) -> reqwest::Url {
        reqwest::Url::parse(&format!("{base}/file")).unwrap()
    }

    fn part_of(dest: &Path) -> PathBuf {
        PathBuf::from(format!("{}.part", dest.display()))
    }

    async fn fetch(
        base: &str,
        dest: &Path,
        reject_html: bool,
    ) -> (Result<u64, AdapterError>, Progress) {
        fetch_with(base, dest, reject_html, Progress::new()).await
    }

    async fn fetch_with(
        base: &str,
        dest: &Path,
        reject_html: bool,
        progress: Progress,
    ) -> (Result<u64, AdapterError>, Progress) {
        let r = tokio::time::timeout(
            WAIT,
            fetch_to_file(
                &file_client(),
                url(base),
                None,
                dest,
                reject_html,
                &progress,
            ),
        )
        .await
        .expect("fetch finishes");
        (r, progress)
    }

    #[test]
    fn sanitize_keeps_last_component() {
        assert_eq!(sanitize_name("../../etc/passwd", 0), "0-passwd");
        assert_eq!(sanitize_name("a\\b\\c.txt", 3), "3-c.txt");
    }

    #[test]
    fn sanitize_maps_unsafe_chars() {
        assert_eq!(
            sanitize_name("my report (v2)!.pdf", 1),
            "1-my_report__v2__.pdf"
        );
        assert_eq!(sanitize_name("사진.png", 2), "2-__.png");
        assert_eq!(sanitize_name("a\nb", 0), "0-a_b");
    }

    #[test]
    fn sanitize_strips_leading_dots() {
        assert_eq!(sanitize_name(".bashrc", 0), "0-bashrc");
        assert_eq!(sanitize_name("...x.txt", 0), "0-x.txt");
    }

    #[test]
    fn sanitize_caps_length_on_char_boundary() {
        let raw = format!("{}{}", "é".repeat(80), "a".repeat(80));
        let name = sanitize_name(&raw, 4);
        let body = name.strip_prefix("4-").unwrap();
        assert_eq!(body.len(), MAX_NAME_BYTES);
        assert!(body.is_char_boundary(body.len()));
        assert_eq!(sanitize_name(&"b".repeat(300), 0).len(), MAX_NAME_BYTES + 2);
    }

    #[test]
    fn sanitize_empty_is_file() {
        assert_eq!(sanitize_name("", 0), "0-file");
        assert_eq!(sanitize_name("..", 1), "1-file");
        assert_eq!(sanitize_name("dir/", 2), "2-file");
    }

    #[test]
    fn inbox_dir_layout() {
        assert_eq!(
            inbox_dir(Path::new("/w"), "chat-th-1", 42),
            PathBuf::from("/w/.moraebot-inbox/chat-th-1/42")
        );
    }

    #[test]
    fn private_dirs_are_0700() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("work");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = inbox_dir(&existing, "k", 7);
        create_private_dir_all(&dir).unwrap();
        for level in [
            existing.join(INBOX_DIR),
            existing.join(INBOX_DIR).join("k"),
            dir.clone(),
        ] {
            assert_eq!(
                std::fs::metadata(&level).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o755
        );
        create_private_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_dir_over_file_is_write_error() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let err = create_private_dir_all(&blocker.join("sub")).unwrap_err();
        assert_eq!(err.code, E_FILE_WRITE);
        assert!(!err.message.contains("blocker"));
    }

    #[test]
    fn check_url_accepts_exact_https_host() {
        let url = check_url(
            "https://files.example.test/a/b?x=1",
            &["files.example.test"],
        )
        .unwrap();
        assert_eq!(url.host_str(), Some("files.example.test"));
    }

    #[test]
    fn check_url_rejects_http_other_host_suffix_trick_and_userinfo() {
        let allowed = ["files.slack.com"];
        for bad in [
            "http://files.slack.com/x",
            "https://other.test/x",
            "https://files.slack.com.evil.test/x",
            "https://u@files.slack.com/x",
            "https://u:p@files.slack.com/x",
            "not a url",
        ] {
            let err = check_url(bad, &allowed).unwrap_err();
            assert_eq!(err.code, E_FILE_HOST);
            assert!(!err.message.contains("files."));
        }
    }

    #[tokio::test]
    async fn fetch_writes_0600_file() {
        let (listener, base) = listen().await;
        let server = serve_once(
            listener,
            vec![b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nabc".to_vec()],
        );
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-a.bin");
        let (r, progress) = fetch(&base, &dest, true).await;
        assert_eq!(r.unwrap(), 3);
        assert_eq!(progress.bytes(), 3);
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!part_of(&dest).exists());
        let head = tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
        assert!(!head.to_ascii_lowercase().contains("authorization"));
    }

    #[tokio::test]
    async fn fetch_records_progress_per_chunk() {
        let (listener, base) = listen().await;
        let server = serve_once(
            listener,
            vec![
                CHUNKED_HEAD.to_vec(),
                chunk(b"first-"),
                chunk(b"second-"),
                chunk(b"third"),
                b"0\r\n\r\n".to_vec(),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-c.bin");
        let (r, progress) = fetch(&base, &dest, false).await;
        assert_eq!(r.unwrap(), 18);
        assert_eq!(progress.bytes(), 18);
        assert_eq!(std::fs::read(&dest).unwrap(), b"first-second-third");
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fetch_over_the_limit_stops_and_removes_the_part_file() {
        let (listener, base) = listen().await;
        let server = serve_once(
            listener,
            vec![
                CHUNKED_HEAD.to_vec(),
                chunk(b"first-"),
                chunk(b"second-"),
                chunk(b"third"),
                b"0\r\n\r\n".to_vec(),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-over.bin");
        let (r, progress) = fetch_with(&base, &dest, false, Progress::with_limit(10)).await;
        assert_eq!(r.unwrap_err().code, E_FILE_TOO_LARGE);
        assert!(progress.bytes() <= 10, "nothing past the limit was written");
        assert!(!dest.exists());
        assert!(!part_of(&dest).exists());
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();

        let (listener, base) = listen().await;
        let server = serve_once(
            listener,
            vec![b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nabc".to_vec()],
        );
        let exact = dir.path().join("0-exact.bin");
        let (r, _) = fetch_with(&base, &exact, false, Progress::with_limit(3)).await;
        assert_eq!(r.unwrap(), 3, "a file of exactly the limit is saved");
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fetch_large_body_is_saved() {
        let (listener, base) = listen().await;
        let block = vec![b'z'; 64 * 1024];
        let mut pieces = vec![CHUNKED_HEAD.to_vec()];
        pieces.extend((0..21 * 16).map(|_| chunk(&block)));
        pieces.push(b"0\r\n\r\n".to_vec());
        let server = serve_once(listener, pieces);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-big.bin");
        let (r, progress) = fetch(&base, &dest, false).await;
        let len = 21 * 1024 * 1024;
        assert_eq!(r.unwrap(), len);
        assert_eq!(progress.bytes(), len);
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), len);
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fetch_network_error_mid_body_is_fetch_error() {
        let (listener, base) = listen().await;
        let server = serve_once(listener, vec![CHUNKED_HEAD.to_vec(), chunk(b"partial")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-cut.bin");
        let (r, _) = fetch(&base, &dest, false).await;
        let err = r.unwrap_err();
        assert_eq!(err.code, E_FILE_FETCH);
        assert!(!err.message.contains("127.0.0.1"));
        assert!(!dest.exists());
        assert!(!part_of(&dest).exists());
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    #[test]
    fn write_error_maps_storage_full() {
        let err = write_error(&io::Error::from(io::ErrorKind::StorageFull));
        assert_eq!(err.code, E_FILE_WRITE);
        assert!(!err.message.contains('/'));
    }

    #[tokio::test(start_paused = true)]
    async fn stall_timeout_fires_after_silence() {
        let progress = Progress::new();
        let start = tokio::time::Instant::now();
        let r = with_stall_timeout(
            std::future::pending::<Result<u64, AdapterError>>(),
            &progress,
        )
        .await;
        assert_eq!(r.unwrap_err().code, E_FILE_FETCH);
        assert_eq!(start.elapsed(), STALL_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn stall_timeout_resets_on_progress() {
        let progress = Arc::new(Progress::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(1);
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<()>(1);
        let watched = Arc::clone(&progress);
        let task = tokio::spawn(async move {
            let fetch = async {
                let mut total = 0;
                while let Some(n) = rx.recv().await {
                    watched.record(n);
                    total += n;
                    ack_tx.send(()).await.unwrap();
                }
                Ok(total)
            };
            with_stall_timeout(fetch, &watched).await
        });
        let start = tokio::time::Instant::now();
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(100)).await;
            tx.send(1).await.unwrap();
            tokio::time::timeout(WAIT, ack_rx.recv())
                .await
                .unwrap()
                .unwrap();
        }
        drop(tx);
        let r = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
        assert_eq!(r.unwrap(), 4);
        assert!(start.elapsed() >= Duration::from_secs(400));
        assert_eq!(progress.bytes(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn stall_timeout_finished_fetch_wins() {
        let progress = Progress::new();
        let fetch = async {
            tokio::time::sleep(STALL_TIMEOUT).await;
            Ok::<_, AdapterError>(5)
        };
        assert_eq!(with_stall_timeout(fetch, &progress).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn fetch_redirect_is_error() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let location = format!(
            "http://127.0.0.1:{}/next",
            target.local_addr().unwrap().port()
        );
        let (listener, base) = listen().await;
        let server = serve_once(
            listener,
            vec![
                format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n")
                    .into_bytes(),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-r.bin");
        let (r, _) = fetch(&base, &dest, false).await;
        assert_eq!(r.unwrap_err().code, E_FILE_FETCH);
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
        let next = target.accept().unwrap_err();
        assert_eq!(
            next.kind(),
            io::ErrorKind::WouldBlock,
            "Location was requested"
        );
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn fetch_existing_dest_is_write_error() {
        let (_listener, base) = listen().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-e.bin");
        std::fs::write(&dest, b"old").unwrap();
        let (r, _) = fetch(&base, &dest, false).await;
        assert_eq!(r.unwrap_err().code, E_FILE_WRITE);
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    }

    #[tokio::test]
    async fn fetch_html_login_page_is_rejected() {
        let (listener, base) = listen().await;
        let page = b"<html>sign in</html>";
        let server = serve_once(
            listener,
            vec![
                format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\n\r\n", page.len())
                    .into_bytes(),
                page.to_vec(),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-h.pdf");
        let (r, _) = fetch(&base, &dest, true).await;
        assert_eq!(r.unwrap_err().code, E_FILE_DENIED);
        assert!(!dest.exists());
        assert!(!part_of(&dest).exists());
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fetch_html_kept_when_not_rejecting() {
        let (listener, base) = listen().await;
        let page = b"<html>page</html>";
        let server = serve_once(
            listener,
            vec![
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\r\n",
                    page.len()
                )
                .into_bytes(),
                page.to_vec(),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-p.html");
        let (r, _) = fetch(&base, &dest, false).await;
        assert_eq!(r.unwrap(), page.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), page);
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fetch_403_is_denied() {
        let (listener, base) = listen().await;
        let server = serve_once(
            listener,
            vec![b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n".to_vec()],
        );
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("0-d.bin");
        let (r, _) = fetch(&base, &dest, false).await;
        assert_eq!(r.unwrap_err().code, E_FILE_DENIED);
        assert!(!dest.exists());
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    /// What the upload listener saw: request head, body, and the listener for a later check.
    type Seen = (String, Vec<u8>, TcpListener);

    /// Accepts one upload, reads head and exactly `Content-Length` body bytes, answers with
    /// `status`, and hands everything to the test.
    fn upload_server(
        listener: TcpListener,
        status: &'static str,
    ) -> tokio::sync::oneshot::Receiver<Seen> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let head = read_head(&mut stream).await;
            let mut body = vec![0u8; content_length(&head)];
            stream.read_exact(&mut body).await.unwrap();
            stream
                .write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
            let _ = tx.send((head, body, listener));
        });
        rx
    }

    fn upload_url(base: &str) -> reqwest::Url {
        reqwest::Url::parse(&format!("{base}/upload")).unwrap()
    }

    #[tokio::test]
    async fn post_bytes_sends_body_without_auth() {
        let (listener, base) = listen().await;
        let seen = upload_server(listener, "200 OK");
        let body: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        let status = tokio::time::timeout(
            WAIT,
            post_bytes(&file_client(), upload_url(&base), body.clone()),
        )
        .await
        .unwrap();
        assert_eq!(status, Some(200));
        let (head, got, listener) = tokio::time::timeout(WAIT, seen).await.unwrap().unwrap();
        let lower = head.to_ascii_lowercase();
        assert!(lower.starts_with("post /upload "));
        assert!(lower.contains(&format!("content-length: {}", body.len())));
        assert!(!lower.contains("authorization"));
        assert!(got == body);
        let second = listener.into_std().unwrap().accept().unwrap_err();
        assert_eq!(
            second.kind(),
            io::ErrorKind::WouldBlock,
            "exactly one request"
        );
    }

    #[tokio::test]
    async fn post_bytes_returns_status() {
        let (listener, base) = listen().await;
        let seen = upload_server(listener, "413 Payload Too Large");
        let status = tokio::time::timeout(
            WAIT,
            post_bytes(&file_client(), upload_url(&base), vec![7u8; 4096]),
        )
        .await
        .unwrap();
        assert_eq!(status, Some(413));
        tokio::time::timeout(WAIT, seen).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn post_bytes_network_error_is_none() {
        let (listener, base) = listen().await;
        let server = serve_once(listener, Vec::new());
        let status = tokio::time::timeout(
            WAIT,
            post_bytes(&file_client(), upload_url(&base), vec![1u8; 16]),
        )
        .await
        .unwrap();
        assert_eq!(status, None);
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn post_bytes_never_logs_body() {
        use tracing_subscriber::fmt::format::FmtSpan;
        let capture = crate::logging::Capture::default();
        let _guard = tracing::subscriber::set_default(crate::logging::subscriber(
            Some("trace"),
            FmtSpan::NEW | FmtSpan::CLOSE,
            capture.clone(),
        ));
        let marker = b"S3LOGMARK-file-bytes-7q2w";
        let body: Vec<u8> = marker.iter().copied().cycle().take(200 * 1024).collect();
        let (listener, base) = listen().await;
        let seen = upload_server(listener, "200 OK");
        let status =
            tokio::time::timeout(WAIT, post_bytes(&file_client(), upload_url(&base), body))
                .await
                .unwrap();
        assert_eq!(status, Some(200));
        tokio::time::timeout(WAIT, seen).await.unwrap().unwrap();
        tracing::info!(target: "moraebot::files", "post-bytes-probe-9d2");
        let text = capture.contents();
        assert!(text.contains("post-bytes-probe-9d2"));
        assert!(!text.contains("S3LOGMARK"));
        assert!(!text.contains("83, 51, 76, 79, 71, 77, 65, 82"));
    }

    struct Dirs {
        _root: tempfile::TempDir,
        work: PathBuf,
        home: PathBuf,
        outside: PathBuf,
    }

    fn dirs() -> Dirs {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let home = root.path().join("home");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&home).unwrap();
        let outside = root.path().to_owned();
        Dirs {
            _root: root,
            work,
            home,
            outside,
        }
    }

    #[test]
    fn outbound_inside_work_dir_ok() {
        let d = dirs();
        let file = d.work.join("report.txt");
        std::fs::write(&file, b"hello").unwrap();
        let (path, size) = check_outbound(&file, &d.work, &d.home).unwrap();
        assert_eq!(path, file.canonicalize().unwrap());
        assert_eq!(size, 5);
    }

    #[test]
    fn outbound_relative_path_resolves_in_work_dir() {
        let name = Path::new("s3-rel-report.txt");
        assert!(
            !name.exists(),
            "the test's own folder must not hold the file"
        );
        let d = dirs();
        std::fs::write(d.work.join(name), b"rel").unwrap();
        let (path, size) = check_outbound(name, &d.work, &d.home).unwrap();
        assert_eq!(path, d.work.canonicalize().unwrap().join(name));
        assert_eq!(size, 3);
    }

    #[test]
    fn outbound_relative_dotdot_escape_rejected() {
        let d = dirs();
        std::fs::write(d.outside.join("s3-outside.txt"), b"x").unwrap();
        let err = check_outbound(Path::new("../s3-outside.txt"), &d.work, &d.home).unwrap_err();
        assert_eq!(err.code, E_FILE_PATH);
    }

    #[test]
    fn outbound_symlink_escape_rejected() {
        let d = dirs();
        let secret = d.outside.join("secret.txt");
        std::fs::write(&secret, b"x").unwrap();
        let link = d.work.join("link.txt");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let err = check_outbound(&link, &d.work, &d.home).unwrap_err();
        assert_eq!(err.code, E_FILE_PATH);
        assert!(!err.message.contains("secret"));
    }

    #[test]
    fn outbound_home_inside_work_dir_rejected() {
        let d = dirs();
        let home = d.work.join(".moraebot-home");
        std::fs::create_dir(&home).unwrap();
        let token = home.join("token");
        std::fs::write(&token, b"x").unwrap();
        let err = check_outbound(&token, &d.work, &home).unwrap_err();
        assert_eq!(err.code, E_FILE_PATH);
    }

    #[test]
    fn outbound_directory_rejected() {
        let d = dirs();
        let sub = d.work.join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(
            check_outbound(&sub, &d.work, &d.home).unwrap_err().code,
            E_FILE_PATH
        );
    }

    #[test]
    fn outbound_large_file_is_accepted() {
        let d = dirs();
        let file = d.work.join("big.bin");
        File::create(&file)
            .unwrap()
            .set_len(21 * 1024 * 1024)
            .unwrap();
        let (_, size) = check_outbound(&file, &d.work, &d.home).unwrap();
        assert_eq!(size, 21 * 1024 * 1024);
    }

    #[test]
    fn outbound_unreadable_file_rejected() {
        let d = dirs();
        let file = d.work.join("locked.txt");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = check_outbound(&file, &d.work, &d.home).unwrap_err();
        assert_eq!(err.code, E_FILE_PATH);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn outbound_fifo_rejected_without_blocking() {
        let d = dirs();
        let fifo = d.work.join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let socket = d.work.join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        for special in [fifo, socket] {
            let (tx, rx) = std::sync::mpsc::channel();
            let (work, home) = (d.work.clone(), d.home.clone());
            std::thread::spawn(move || {
                let _ = tx.send(check_outbound(&special, &work, &home));
            });
            let r = rx
                .recv_timeout(WAIT)
                .expect("check_outbound must not block");
            assert_eq!(r.unwrap_err().code, E_FILE_PATH);
        }
    }

    fn saved(path: &str, name: &str, mime: Option<&str>, bytes: u64) -> SavedFile {
        SavedFile {
            path: PathBuf::from(path),
            name: name.to_owned(),
            mime: mime.map(str::to_owned),
            bytes,
        }
    }

    #[test]
    fn prompt_block_exact_format() {
        let files = [
            saved(
                "/w/.moraebot-inbox/k/1/0-a.txt",
                "0-a\nb.txt",
                Some("text/plain\nx"),
                3,
            ),
            saved("/w/.moraebot-inbox/k/1/1-b.bin", "1-b.bin", None, 0),
        ];
        assert_eq!(
            prompt_with_files("hi", &files),
            "hi\n\n[Attached files, saved on disk]\n\
             - /w/.moraebot-inbox/k/1/0-a.txt (name: 0-a_b.txt, type: text/plain_x, 3 bytes)\n\
             - /w/.moraebot-inbox/k/1/1-b.bin (name: 1-b.bin, type: unknown, 0 bytes)"
        );
    }

    #[test]
    fn prompt_without_files_is_unchanged() {
        assert_eq!(prompt_with_files("  exact text\n", &[]), "  exact text\n");
        assert_eq!(prompt_with_files("", &[]), "");
    }

    #[test]
    fn prompt_with_text_and_files_has_both() {
        let files = [
            saved("/w/a", "0-a", None, 1),
            saved("/w/b", "1-b", Some("image/png"), 2),
        ];
        let prompt = prompt_with_files("look at these", &files);
        let (text, block) = prompt.split_once("\n\n").unwrap();
        assert_eq!(text, "look at these");
        assert!(block.starts_with("[Attached files, saved on disk]\n"));
        assert!(block.contains("- /w/a (name: 0-a, type: unknown, 1 bytes)"));
        assert!(block.contains("- /w/b (name: 1-b, type: image/png, 2 bytes)"));
    }

    #[test]
    fn prompt_with_only_files_has_no_leading_blank_lines() {
        let files = [saved("/w/a", "0-a", None, 1)];
        for text in ["", "  \n "] {
            assert_eq!(
                prompt_with_files(text, &files),
                "[Attached files, saved on disk]\n- /w/a (name: 0-a, type: unknown, 1 bytes)"
            );
        }
    }

    #[test]
    fn saved_file_round_trips_and_debug_hides_name() {
        let file = saved(
            "/w/secret-name.txt",
            "0-secret-name.txt",
            Some("text/plain"),
            9,
        );
        let json = serde_json::to_string(std::slice::from_ref(&file)).unwrap();
        assert!(serde_json::from_str::<Vec<SavedFile>>(&json).unwrap() == [file.clone()]);
        assert!(!format!("{file:?}").contains("secret"));
        let out: OutFile =
            serde_json::from_str(r#"{"path":"/w/x","name":"x","sent":false,"skipped":false}"#)
                .unwrap();
        assert!(!out.attempting);
        assert_eq!(out.attempts, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn progress_record_moves_last_and_counts_bytes() {
        let p = Progress::new();
        p.record(10);
        let first = p.last();
        tokio::time::advance(Duration::from_secs(7)).await;
        p.record(5);
        assert_eq!(p.last() - first, Duration::from_secs(7));
        assert_eq!(p.bytes(), 15);
    }
}
