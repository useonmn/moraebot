//! The lead's omp extension: the shipped TypeScript source, the files it shares with moraebot
//! under `<home>/omp`, and the load check.

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
