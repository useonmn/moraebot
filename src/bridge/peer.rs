//! The peer uid check on the bridge socket. The socket's 0600 mode does not stop other
//! processes of the same user from connecting, and a socket file can be swapped while
//! its folder is writable, so each side also checks that the peer runs as its own uid.
//!
//! tokio's `UnixStream::peer_cred()` reads `SO_PEERCRED` on Linux and `getpeereid` on
//! macOS; the tests below run on both CI runners.

use std::fmt;
use std::io;

use tokio::net::UnixStream;

use super::E_BRIDGE_PEER;

#[derive(Debug)]
pub enum PeerError {
    /// The OS did not report the peer's credentials.
    Unreadable(io::Error),
    Mismatch {
        peer: u32,
        own: u32,
    },
}

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeerError::Unreadable(err) => {
                write!(f, "{E_BRIDGE_PEER}: cannot read the peer uid: {err}")
            }
            PeerError::Mismatch { peer, own } => write!(
                f,
                "{E_BRIDGE_PEER}: the peer runs as uid {peer}, this process as uid {own}"
            ),
        }
    }
}

impl std::error::Error for PeerError {}

/// The effective uid of the process on the other end of `stream`.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    stream.peer_cred().map(|cred| cred.uid())
}

/// The effective uid of this process.
pub fn own_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory of ours.
    unsafe { libc::geteuid() }
}

/// `Ok` when the peer of `stream` runs as this process's effective uid.
pub fn check_peer(stream: &UnixStream) -> Result<(), PeerError> {
    let peer = peer_uid(stream).map_err(PeerError::Unreadable)?;
    check_uid(peer, own_uid())
}

fn check_uid(peer: u32, own: u32) -> Result<(), PeerError> {
    if peer == own {
        Ok(())
    } else {
        Err(PeerError::Mismatch { peer, own })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn both_ends_of_a_real_connection_see_this_uid() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let (client, accepted) = tokio::join!(UnixStream::connect(&path), listener.accept());
        let client = client.expect("connect");
        let (server, _) = accepted.expect("accept");

        assert_eq!(peer_uid(&server).expect("server side"), own_uid());
        assert_eq!(peer_uid(&client).expect("client side"), own_uid());
        check_peer(&server).expect("bridge accepts the core");
        check_peer(&client).expect("core accepts the bridge");
    }

    #[test]
    fn another_uid_is_refused() {
        let err = check_uid(0, 1000).expect_err("mismatch");
        assert!(matches!(err, PeerError::Mismatch { peer: 0, own: 1000 }));
        assert!(err.to_string().starts_with(E_BRIDGE_PEER));
        check_uid(1000, 1000).expect("same uid");
    }
}
