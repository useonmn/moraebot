//! The Linux background service: a systemd --user unit that keeps `moraebot run` going,
//! the counterpart of `launchd` on macOS.

/// Unit name (without `.service`) used when `--unit` is not given.
pub const DEFAULT_UNIT: &str = "moraebot";
pub const E_UNIT_PATH: &str = "E_UNIT_PATH"; // a path a unit file cannot hold
pub const E_SYSTEMCTL: &str = "E_SYSTEMCTL"; // systemctl could not run or failed
