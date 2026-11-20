//! The link between the core and a messenger bridge (issue #40).
//!
//! A bridge is a separate long-running process that owns one messenger connection and
//! listens on a unix socket; the core connects to it and talks JSON lines. The wire
//! contract is `docs/bridge.md`. This module holds the parts both sides share:
//! [`proto`] (frame types and the line codec), [`peer`] (the peer uid check) and
//! [`socket`] (the default socket path and its private folder) and [`spool`] (the
//! bridge's on-disk inbound spool). [`client`] is the core's end of the link, the
//! [`BridgeAdapter`].

pub mod client;
pub mod peer;
pub mod proto;
pub mod socket;
pub mod spool;

pub use client::BridgeAdapter;
pub use peer::{check_peer, peer_uid};
pub use proto::{FrameReader, MAX_LINE, PROTO_VERSION};
pub use spool::Spool;

/// The peer speaks another `proto` version.
pub const E_BRIDGE_PROTO: &str = "E_BRIDGE_PROTO";
/// The bridge already serves another core.
pub const E_BRIDGE_BUSY: &str = "E_BRIDGE_BUSY";
/// The frame type or the request is not supported by this side.
pub const E_BRIDGE_UNSUPPORTED: &str = "E_BRIDGE_UNSUPPORTED";
/// A request misses a field or carries an invalid one.
pub const E_BRIDGE_BAD_REQUEST: &str = "E_BRIDGE_BAD_REQUEST";
/// The peer runs as another uid (logged only, the connection is closed).
pub const E_BRIDGE_PEER: &str = "E_BRIDGE_PEER";
/// The link is down (made by the core).
pub const E_BRIDGE_DOWN: &str = "E_BRIDGE_DOWN";
/// A later hello named another platform than the one the core started with (made by the core).
pub const E_BRIDGE_PLATFORM: &str = "E_BRIDGE_PLATFORM";
/// The bridge's inbound spool failed on the file system, or dropped a message.
pub const E_BRIDGE_SPOOL: &str = "E_BRIDGE_SPOOL";
/// The socket path is unusable: relative, too long for `sockaddr_un`, or its folder is
/// not a private folder of this user.
pub const E_BRIDGE_SOCKET: &str = "E_BRIDGE_SOCKET";
