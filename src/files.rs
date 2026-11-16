//! Received and outbound files: limits, names, paths, downloads. Messenger-agnostic.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

/// Bytes received so far and when the last chunk arrived, for the stall watchdog.
pub struct Progress {
    bytes: AtomicU64,
    last: Mutex<tokio::time::Instant>,
}

impl Progress {
    pub fn new() -> Self {
        Self { bytes: AtomicU64::new(0), last: Mutex::new(tokio::time::Instant::now()) }
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

#[cfg(test)]
mod tests {
    use super::*;

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
