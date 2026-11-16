//! Lanes: separate headless omp children started by the lead through host tools. Messenger-agnostic.

/// Most lanes that may run a turn at the same time.
pub const MAX_RUNNING_LANES: usize = 3;
/// Inbox `platform` of rows that carry a lane's result to the lead.
pub const LANE_PLATFORM: &str = "lane";
/// Host tool that starts a new lane.
pub const LANE_START_TOOL: &str = "lane_start";
/// Host tool that sends a follow-up to an idle lane.
pub const LANE_MESSAGE_TOOL: &str = "lane_message";
/// Host tool that lists lanes and their state.
pub const LANE_STATUS_TOOL: &str = "lane_status";

/// A lane tool call had missing or invalid arguments.
pub const E_LANE_ARGS: &str = "E_LANE_ARGS";
/// No lane has the given id.
pub const E_LANE_UNKNOWN: &str = "E_LANE_UNKNOWN";
/// The lane is still running a turn.
pub const E_LANE_BUSY: &str = "E_LANE_BUSY";
/// `MAX_RUNNING_LANES` lanes are already running.
pub const E_LANE_LIMIT: &str = "E_LANE_LIMIT";
/// The lane was running when moraebot stopped or restarted.
pub const E_LANE_INTERRUPTED: &str = "E_LANE_INTERRUPTED";
/// moraebot is shutting down and starts no new lane.
pub const E_LANE_STOPPING: &str = "E_LANE_STOPPING";
