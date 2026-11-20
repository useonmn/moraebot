use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::format::{FmtSpan, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, filter, fmt};

pub const E_LOG_DIR_NOT_WRITABLE: &str = "E_LOG_DIR_NOT_WRITABLE";

const MAX_LOG_FILES: usize = 15;

/// File name prefix of the core's daily log.
pub const CORE_LOG_PREFIX: &str = "moraebot.log";

fn build(
    home: &Path,
    prefix: &str,
) -> Result<(impl tracing::Subscriber + Send + Sync, WorkerGuard)> {
    let dir = home.join("logs");
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .max_log_files(MAX_LOG_FILES)
        .build(&dir)
        .with_context(|| {
            format!(
                "{E_LOG_DIR_NOT_WRITABLE}: cannot write logs to {}",
                dir.display()
            )
        })?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let spec = std::env::var("RUST_LOG").ok();
    Ok((subscriber(spec.as_deref(), FmtSpan::NONE, writer), guard))
}

/// The log pipeline: `RUST_LOG`-style filter, then the cap on other crates, then the line
/// format. moraebot's own lines are written in full; another crate never gets a span, its
/// events below WARN are dropped, and its WARN/ERROR events become one fixed line.
pub(crate) fn subscriber<W>(
    spec: Option<&str>,
    span_events: FmtSpan,
    writer: W,
) -> impl tracing::Subscriber + Send + Sync + use<W>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    Registry::default()
        .with(env_filter(spec))
        .with(filter::filter_fn(third_party_cap))
        .with(
            fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_span_events(span_events)
                .event_format(ThirdPartyLine::default()),
        )
}

/// A non-empty valid spec, else `info`.
fn env_filter(spec: Option<&str>) -> EnvFilter {
    spec.filter(|s| !s.trim().is_empty())
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| EnvFilter::new("info"))
}

/// The moraebot library and binary.
fn is_moraebot(target: &str) -> bool {
    target == "moraebot" || target.starts_with("moraebot::")
}

/// moraebot: everything; any other crate: events at WARN or above, never a span.
fn third_party_cap(meta: &tracing::Metadata<'_>) -> bool {
    is_moraebot(meta.target()) || (meta.is_event() && *meta.level() <= tracing::Level::WARN)
}

/// First `::` segment of a target when it is 1-32 characters of `[A-Za-z0-9_-]`, else `other`.
fn source_crate(target: &str) -> &str {
    let first = target.split("::").next().unwrap_or_default();
    let ok = (1..=32).contains(&first.len())
        && first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok { first } else { "other" }
}

/// moraebot events keep the default format; another crate's event is written as
/// `<time> <LEVEL> third_party: event suppressed source=<crate>` without its message,
/// fields, span scope or full target.
#[derive(Default)]
struct ThirdPartyLine {
    inner: fmt::format::Format,
}

impl<S, N> FormatEvent<S, N> for ThirdPartyLine
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        if is_moraebot(meta.target()) {
            return self.inner.format_event(ctx, writer, event);
        }
        fmt::time::SystemTime.format_time(&mut writer)?;
        writeln!(
            writer,
            " {:>5} third_party: event suppressed source={}",
            meta.level().as_str(),
            source_crate(meta.target())
        )
    }
}

/// Test-only log sink: keeps every written byte and wakes `wait_for` after each write.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct Capture {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    written: std::sync::Arc<tokio::sync::Notify>,
}

/// Creating a `Capture` registers a second `Dispatch` that lives for the rest of the process.
/// tracing-core 0.1.36 computes a new callsite's interest from the registering thread's
/// default while only one `Dispatch` is registered, so a parallel test without a subscriber
/// can cache `Interest::never` for a callsite a capturing test asserts on and its event is
/// dropped (tokio-rs/tracing#3611). With two registered, interest comes from all of them.
#[cfg(test)]
impl Default for Capture {
    fn default() -> Self {
        static SECOND_DISPATCH: std::sync::LazyLock<tracing::Dispatch> =
            std::sync::LazyLock::new(|| {
                tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default())
            });
        std::sync::LazyLock::force(&SECOND_DISPATCH);
        Self {
            buf: Default::default(),
            written: Default::default(),
        }
    }
}

#[cfg(test)]
impl Capture {
    pub(crate) fn contents(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
    }

    /// True once the captured text contains `needle` at least `count` times, false after `limit`.
    pub(crate) async fn wait_for(
        &self,
        needle: &str,
        count: usize,
        limit: std::time::Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        let seen = || self.contents().matches(needle).count() >= count;
        loop {
            let notified = self.written.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if seen() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return seen();
            }
        }
    }
}

#[cfg(test)]
impl std::io::Write for Capture {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(data);
        self.written.notify_waiters();
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl<'w> MakeWriter<'w> for Capture {
    type Writer = Capture;

    fn make_writer(&'w self) -> Self::Writer {
        self.clone()
    }
}

/// Writes the process's log to `<home>/logs/<prefix>.<date>`.
pub fn init(home: &Path, prefix: &str) -> Result<WorkerGuard> {
    let (subscriber, guard) = build(home, prefix)?;
    subscriber.try_init()?;
    Ok(guard)
}

pub fn redact(text: &str) -> String {
    format!("len={}", text.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn line_lands_in_daily_file_named_by_the_prefix() {
        for prefix in [CORE_LOG_PREFIX, "slack-bridge.log"] {
            let home = tempfile::tempdir().unwrap();
            let (subscriber, guard) = build(home.path(), prefix).unwrap();
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!("logging-test-line-7f3a");
            });
            drop(guard);

            let logs = home.path().join("logs");
            let names: Vec<_> = fs::read_dir(&logs)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(names.len(), 1, "{names:?}");
            let name = names[0].file_name().unwrap().to_string_lossy().into_owned();
            assert!(name.starts_with(prefix), "{name}");
            assert!(
                fs::read_to_string(&names[0])
                    .unwrap()
                    .contains("logging-test-line-7f3a")
            );
        }
    }

    #[test]
    fn redact_returns_length_only() {
        assert_eq!(redact("secret body text"), "len=16");
        assert_eq!(redact("안녕하세요"), "len=5");
        assert!(!redact("secret body text").contains("secret"));
    }

    use tracing::Level;

    const WAIT: std::time::Duration = std::time::Duration::from_secs(30);

    fn capture_subscriber(
        spec: Option<&str>,
        span_events: FmtSpan,
    ) -> (impl tracing::Subscriber + Send + Sync, Capture) {
        let capture = Capture::default();
        (subscriber(spec, span_events, capture.clone()), capture)
    }

    macro_rules! assert_capped {
        ($target:literal) => {
            assert!(tracing::span!(target: $target, Level::ERROR, "s").is_disabled());
            assert!(tracing::span!(target: $target, Level::INFO, "s").is_disabled());
            assert!(!tracing::enabled!(kind: tracing::metadata::Kind::EVENT, target: $target, Level::INFO));
            assert!(tracing::enabled!(kind: tracing::metadata::Kind::EVENT, target: $target, Level::WARN));
        };
    }

    #[test]
    fn third_party_targets_are_capped_whatever_rust_log() {
        let (sub, _capture) = capture_subscriber(Some("trace"), FmtSpan::NONE);
        tracing::subscriber::with_default(sub, || {
            assert_capped!("serenity::http::client");
            assert_capped!("serenity::gateway::ws");
            assert_capped!("reqwest::connect");
            assert_capped!("hyper_util::client::legacy");
            assert_capped!("tungstenite::protocol");
            assert!(tracing::enabled!(target: "moraebot::dispatch", Level::TRACE));
            assert!(!tracing::span!(target: "moraebot::dispatch", Level::TRACE, "s").is_disabled());
        });
        let (sub, _capture) = capture_subscriber(Some("serenity=trace"), FmtSpan::NONE);
        tracing::subscriber::with_default(sub, || {
            assert!(
                tracing::span!(target: "serenity::http::client", Level::TRACE, "s").is_disabled()
            );
            assert!(
                tracing::span!(target: "serenity::http::client", Level::ERROR, "s").is_disabled()
            );
        });
    }

    #[test]
    fn default_spec_keeps_moraebot_info() {
        for spec in [None, Some(""), Some("not a [valid spec")] {
            let (sub, _capture) = capture_subscriber(spec, FmtSpan::NONE);
            tracing::subscriber::with_default(sub, || {
                assert!(
                    tracing::enabled!(target: "moraebot::dispatch", Level::INFO),
                    "{spec:?}"
                );
                assert!(
                    !tracing::enabled!(target: "moraebot::dispatch", Level::DEBUG),
                    "{spec:?}"
                );
            });
        }
    }

    #[derive(Debug)]
    #[allow(dead_code)]
    struct FakeRequest {
        filename: String,
        data: Vec<u8>,
    }

    const MARKER: &str = "S3LOGMARK-file-bytes-7q2w";
    const MARKER_DEBUG: &str = "83, 51, 76, 79, 71, 77, 65, 82";
    const SECRET_NAME: &str = "s3-secret-name-5k8.txt";

    #[test]
    fn capture_never_contains_file_bytes() {
        let (sub, capture) = capture_subscriber(Some("trace"), FmtSpan::NEW | FmtSpan::CLOSE);
        tracing::subscriber::with_default(sub, || {
            let fake = FakeRequest {
                filename: SECRET_NAME.to_owned(),
                data: MARKER.as_bytes().to_vec(),
            };
            let span =
                tracing::info_span!(target: "serenity::http::client", "request", req = ?fake);
            let _entered = span.enter();
            tracing::info!(target: "moraebot::files", bytes = 25, "log-capture-probe-3k1");
        });
        let text = capture.contents();
        assert!(text.contains("log-capture-probe-3k1"));
        for leak in [MARKER, MARKER_DEBUG, SECRET_NAME] {
            assert!(!text.contains(leak), "leaked {leak}");
        }
    }

    const PAYLOAD: &str = r#"{"content":"S3GWMARK-7p4z","attachments":[{"filename":"s3-secret-name-5k8.txt","url":"https://cdn.discordapp.com/attachments/111/222/s3-secret-name-5k8.txt?ex=0&hm=S3GWMARK-7p4z"}]}"#;
    const SIGNED_URL: &str = "https://cdn.discordapp.com/attachments/111/222/s3-secret-name-5k8.txt?ex=0&hm=S3GWMARK-7p4z";
    const LEAKS: [&str; 4] = [
        "S3GWMARK-7p4z",
        SECRET_NAME,
        "cdn.discordapp.com",
        "test-token-111",
    ];

    macro_rules! warn_and_error {
        ($target:literal) => {
            let payload = PAYLOAD;
            tracing::warn!(target: $target, url = SIGNED_URL, token = "test-token-111", "text: {payload}");
            tracing::error!(target: $target, url = SIGNED_URL, token = "test-token-111", "text: {payload}");
        };
    }

    #[test]
    fn third_party_event_is_one_fixed_line() {
        let (sub, capture) = capture_subscriber(Some("trace"), FmtSpan::NEW | FmtSpan::CLOSE);
        tracing::subscriber::with_default(sub, || {
            warn_and_error!("serenity::gateway::ws");
            warn_and_error!("serenity::gateway::bridge::shard_runner");
            warn_and_error!("reqwest::connect");
            warn_and_error!("hyper_util::client::legacy");
            warn_and_error!("h2::codec");
            warn_and_error!("tungstenite::protocol");
            warn_and_error!("tokio_tungstenite");
            warn_and_error!("omp_rpc::client");
            warn_and_error!("log");
            warn_and_error!("bad target\nS3GWMARK-7p4z");
        });
        let text = capture.contents();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 20, "{text}");
        let sources = [
            "serenity",
            "serenity",
            "reqwest",
            "hyper_util",
            "h2",
            "tungstenite",
            "tokio_tungstenite",
            "omp_rpc",
            "log",
            "other",
        ];
        for (i, line) in lines.iter().enumerate() {
            let level = if i % 2 == 0 { "WARN" } else { "ERROR" };
            let (time, rest) = line.split_once(' ').unwrap();
            assert!(!time.is_empty() && !time.contains(char::is_whitespace));
            assert_eq!(
                rest.trim_start_matches(' '),
                format!(
                    "{level} third_party: event suppressed source={}",
                    sources[i / 2]
                )
            );
        }
        for leak in LEAKS {
            assert!(!text.contains(leak), "leaked {leak}");
        }
    }

    #[test]
    fn moraebot_lines_keep_message_and_fields() {
        let (sub, capture) = capture_subscriber(None, FmtSpan::NONE);
        tracing::subscriber::with_default(sub, || {
            let span =
                tracing::info_span!(target: "moraebot::dispatch", "turn", conversation = "k1");
            let _entered = span.enter();
            tracing::info!(target: "moraebot::dispatch", row = 7, "message queued");
        });
        let text = capture.contents();
        let line = text
            .lines()
            .find(|l| l.contains("message queued"))
            .expect("event line");
        assert!(line.contains("row=7"), "{line}");
        assert!(line.contains("conversation=\"k1\""), "{line}");
        assert!(line.contains("moraebot::dispatch"), "{line}");
    }

    /// Reads one request head up to the blank line.
    async fn read_head(stream: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                return;
            }
            head.push(byte[0]);
        }
    }

    #[tokio::test]
    async fn serenity_gateway_payload_never_logged() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let (sub, capture) = capture_subscriber(Some("trace"), FmtSpan::NEW | FmtSpan::CLOSE);
        let _guard = tracing::subscriber::set_default(sub);

        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_port = gateway.local_addr().unwrap().port();
        let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_port = api.local_addr().unwrap().port();

        let api_task = tokio::spawn(async move {
            let body = format!(r#"{{"url":"ws://127.0.0.1:{gateway_port}"}}"#);
            loop {
                let Ok((mut stream, _)) = api.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    read_head(&mut stream).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });

        let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
        let gateway_task = tokio::spawn(async move {
            let (stream, _) = gateway.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // No "s": serenity 0.12.5 parses a dispatch with an incomplete body as
            // `Event::Unknown`, but a dispatch without its sequence number fails
            // `GatewayEvent` parsing, so `recv_json` logs the raw frame at WARN.
            let frame = format!(r#"{{"op":0,"t":"MESSAGE_CREATE","d":{PAYLOAD}}}"#);
            ws.send(tokio_tungstenite::tungstenite::Message::Text(frame.into()))
                .await
                .unwrap();
            let _ = sent_tx.send(());
            while let Some(Ok(_)) = ws.next().await {}
        });

        let http = serenity::http::HttpBuilder::new("test-token-111")
            .proxy(format!("http://127.0.0.1:{api_port}"))
            .ratelimiter_disabled(true)
            .build();
        let builder = serenity::client::ClientBuilder::new_with_http(
            http,
            serenity::prelude::GatewayIntents::empty(),
        );
        let mut client = tokio::time::timeout(WAIT, std::future::IntoFuture::into_future(builder))
            .await
            .expect("client builds")
            .expect("client builds");
        let client_task = tokio::spawn(async move {
            let _ = client.start().await;
        });

        tokio::time::timeout(WAIT, sent_rx)
            .await
            .expect("gateway reached")
            .unwrap();
        let capped = capture
            .wait_for(
                "WARN third_party: event suppressed source=serenity",
                2,
                WAIT,
            )
            .await;
        assert!(
            capped,
            "no capped serenity warning; raw payload logged: {}",
            capture.contents().contains("Err deserializing text")
        );
        client_task.abort();
        gateway_task.abort();
        api_task.abort();

        let text = capture.contents();
        for leak in LEAKS.into_iter().chain(["127.0.0.1"]) {
            assert!(!text.contains(leak), "leaked {leak}");
        }
    }

    #[test]
    fn unwritable_log_dir_returns_code() {
        let root = tempfile::tempdir().unwrap();
        let not_a_dir = root.path().join("home");
        fs::write(&not_a_dir, b"not a directory").unwrap();
        let err = build(&not_a_dir, CORE_LOG_PREFIX)
            .err()
            .expect("build must fail");
        assert!(format!("{err:#}").starts_with(E_LOG_DIR_NOT_WRITABLE));
    }
}
