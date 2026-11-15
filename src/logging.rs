use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

pub const E_LOG_DIR_NOT_WRITABLE: &str = "E_LOG_DIR_NOT_WRITABLE";

const MAX_LOG_FILES: usize = 15;

fn build(home: &Path) -> Result<(impl tracing::Subscriber + Send + Sync, WorkerGuard)> {
    let dir = home.join("logs");
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("moraebot.log")
        .max_log_files(MAX_LOG_FILES)
        .build(&dir)
        .with_context(|| {
            format!(
                "{E_LOG_DIR_NOT_WRITABLE}: cannot write logs to {}",
                dir.display()
            )
        })?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let subscriber = Registry::default()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(writer).with_ansi(false));
    Ok((subscriber, guard))
}

pub fn init(home: &Path) -> Result<WorkerGuard> {
    let (subscriber, guard) = build(home)?;
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
    fn line_lands_in_daily_file() {
        let home = tempfile::tempdir().unwrap();
        let (subscriber, guard) = build(home.path()).unwrap();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("logging-test-line-7f3a");
        });
        drop(guard);

        let logs = home.path().join("logs");
        let file = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("moraebot.log")
            })
            .expect("a moraebot.log file");
        assert!(fs::read_to_string(file)
            .unwrap()
            .contains("logging-test-line-7f3a"));
    }

    #[test]
    fn redact_returns_length_only() {
        assert_eq!(redact("secret body text"), "len=16");
        assert_eq!(redact("안녕하세요"), "len=5");
        assert!(!redact("secret body text").contains("secret"));
    }

    #[test]
    fn unwritable_log_dir_returns_code() {
        let root = tempfile::tempdir().unwrap();
        let not_a_dir = root.path().join("home");
        fs::write(&not_a_dir, b"not a directory").unwrap();
        let err = build(&not_a_dir).err().expect("build must fail");
        assert!(format!("{err:#}").starts_with(E_LOG_DIR_NOT_WRITABLE));
    }
}
