//! The Slack bridge (`moraebot-slack-bridge`): Slack Socket Mode and Web API on one
//! side, the bridge socket to the core on the other (see `docs/bridge.md`).
//!
//! Start order: the state folder lock (one bridge per moraebot home), the spool, the
//! Slack link, then the socket. The link's `auth.test` is retried in the background
//! until it passes, so the socket answers `hello` while Slack is unreachable.

pub mod config;
pub mod server;
pub mod slack;
pub mod testing;

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::adapter::{AdapterError, MessengerAdapter};
use crate::bridge::peer::own_uid;
use crate::bridge::spool::{Spool, SpoolError};
use crate::config::ConfigError;
use config::SlackBridgeConfig;
use server::{Server, ServerOptions};
use slack::{HttpTransport, SlackLink, SlackTransport, unix_now};

/// The bridge name in `hello`, and the binary's name.
pub const BRIDGE_NAME: &str = "moraebot-slack-bridge";
/// Folder under the moraebot home that holds the lock and the spool.
pub const STATE_DIR: &str = "slack-bridge";
/// File name prefix of the bridge's daily log in `<home>/logs`.
pub const LOG_PREFIX: &str = "slack-bridge.log";

/// Another bridge already runs on this moraebot home.
pub const E_BRIDGE_RUNNING: &str = "E_BRIDGE_RUNNING";
/// The bridge is shutting down (`bye` code).
pub const E_BRIDGE_STOPPING: &str = "E_BRIDGE_STOPPING";
/// The bridge's state folder or lock file failed on the file system.
pub const E_BRIDGE_STATE: &str = "E_BRIDGE_STATE";

/// Why the bridge could not start or keep running.
#[derive(Debug)]
pub struct BridgeError {
    pub code: &'static str,
    pub message: String,
}

impl BridgeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn from_config(err: ConfigError) -> Self {
        Self::new(err.code, err.message)
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BridgeError {}

impl From<AdapterError> for BridgeError {
    fn from(err: AdapterError) -> Self {
        Self::new(err.code, err.message)
    }
}

impl From<SpoolError> for BridgeError {
    fn from(err: SpoolError) -> Self {
        Self::new(
            crate::bridge::E_BRIDGE_SPOOL,
            format!("{}: {}", err.context, err.source),
        )
    }
}

/// Everything [`Bridge::start`] needs; [`run`] fills it from the config, tests from fakes.
pub struct BridgeParts {
    /// Holds the lock file and the spool folder.
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    pub channel_id: String,
    pub transport: Arc<dyn SlackTransport>,
    /// The uid a core must run as.
    pub expected_uid: u32,
}

pub struct Bridge {
    _lock: File,
    link: Arc<SlackLink>,
    server: Server,
}

impl Bridge {
    /// Locks the state folder, opens the spool, starts the Slack link and binds the
    /// socket. A second bridge on the same state folder fails with `E_BRIDGE_RUNNING`.
    pub async fn start(parts: BridgeParts) -> Result<Self, BridgeError> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parts.state_dir)
            .map_err(|err| {
                BridgeError::new(
                    E_BRIDGE_STATE,
                    format!("cannot create {}: {err}", parts.state_dir.display()),
                )
            })?;
        let lock = take_lock(&parts.state_dir.join("lock"))?;
        let spool = Arc::new(Spool::open(&parts.state_dir.join("spool"), unix_now())?);
        // The owner and allow list come from each core's hello_ok, not from here.
        let link = Arc::new(SlackLink::with_transport(
            parts.transport,
            String::new(),
            parts.channel_id.clone(),
        ));
        let (appended, appended_rx) = watch::channel(0);
        let bot_user_id = link.start_spooling(Arc::clone(&spool), appended)?;
        let bound = Server::bind(
            ServerOptions {
                socket: parts.socket,
                channel_id: parts.channel_id,
                expected_uid: parts.expected_uid,
            },
            Arc::clone(&link),
            spool,
            appended_rx,
            bot_user_id,
        );
        let server = match bound {
            Ok(server) => server,
            Err(err) => {
                link.shutdown().await;
                return Err(err);
            }
        };
        Ok(Self {
            _lock: lock,
            link,
            server,
        })
    }

    pub fn socket(&self) -> &Path {
        self.server.socket()
    }

    /// Says `bye` to the core, removes the socket file and closes the Slack link.
    pub async fn shutdown(self) {
        self.server.shutdown().await;
        self.link.shutdown().await;
    }
}

/// Takes an exclusive `flock` on `path` (created with mode 0600), held while the file
/// stays open.
fn take_lock(path: &Path) -> Result<File, BridgeError> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|err| {
            BridgeError::new(
                E_BRIDGE_STATE,
                format!("cannot open {}: {err}", path.display()),
            )
        })?;
    // SAFETY: flock only reads the descriptor, which `file` keeps open.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        let err = std::io::Error::last_os_error();
        return Err(if err.kind() == std::io::ErrorKind::WouldBlock {
            BridgeError::new(
                E_BRIDGE_RUNNING,
                format!("another bridge holds {}", path.display()),
            )
        } else {
            BridgeError::new(
                E_BRIDGE_STATE,
                format!("cannot lock {}: {err}", path.display()),
            )
        });
    }
    Ok(file)
}

/// Runs the bridge of `config` on the moraebot home `home` until SIGTERM or SIGINT.
pub async fn run(config: SlackBridgeConfig, home: &Path) -> Result<(), BridgeError> {
    let transport = HttpTransport::new(&config.bot_token_file, &config.app_token_file)?;
    let signal_error = |err: std::io::Error| {
        BridgeError::new(E_BRIDGE_STATE, format!("cannot listen for signals: {err}"))
    };
    let mut terminate = signal(SignalKind::terminate()).map_err(signal_error)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(signal_error)?;
    let bridge = Bridge::start(BridgeParts {
        state_dir: home.join(STATE_DIR),
        socket: config.socket,
        channel_id: config.channel_id,
        transport: Arc::new(transport),
        expected_uid: own_uid(),
    })
    .await?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    tracing::info!("shutdown signal received");
    bridge.shutdown().await;
    Ok(())
}
