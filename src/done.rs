//! Lane done checks: the Done line a lane starts with, the `lane_done` claim, and the
//! gateway-run checks of it (Dori mode style). Messenger-agnostic.

use std::time::Duration;

/// Kill deadline of one `command` signal (Dori mode `COMMAND_TIMEOUT_MS`).
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
/// A check detail is cut to this many characters (Dori mode `short()`).
pub const DETAIL_CHARS: usize = 120;
/// The host tool a lane calls to claim its work is done.
pub const LANE_DONE_TOOL: &str = "lane_done";

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
