//! Library entry of the gateway: wires the store, the omp manager, a messenger adapter
//! and the dispatcher, and shuts them down in order.
//!
//! The adapter is injected, so tests run the whole gateway with `FakeAdapter` while
//! `moraebot run` passes the configured messenger's adapter.
//!
//! Shutdown order: stop taking events from the adapter, let the event being handled
//! finish its store write, drop the in-flight turn, stop the lanes, kill and reap the
//! lead and every lane child, then mark the interrupted rows and lanes `needs_check` so
//! nothing is re-prompted.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinError;

use crate::adapter::{AdapterError, MessengerAdapter};
use crate::config::{Config, ConfigError};
use crate::dispatch::Dispatcher;
use crate::hints;
use crate::lead;
use crate::omp::OmpManager;
use crate::onboarding;
use crate::store::{Store, StoreError};

/// File name of the inbox database inside the home folder.
pub const STORE_FILE: &str = "inbox.sqlite";
/// The dispatcher task panicked or was cancelled.
pub const E_GATEWAY_TASK: &str = "E_GATEWAY_TASK";
/// The process could not listen for termination signals.
pub const E_GATEWAY_SIGNAL: &str = "E_GATEWAY_SIGNAL";
/// The gateway did not finish shutting down within its time bound.
pub const E_SHUTDOWN_TIMEOUT: &str = "E_SHUTDOWN_TIMEOUT";

/// Capacity of the queue between the adapter and the dispatcher.
const EVENT_QUEUE: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Adapter(#[from] AdapterError),
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("{E_GATEWAY_TASK}: dispatcher task failed: {0}")]
    Task(JoinError),
    #[error("{E_GATEWAY_SIGNAL}: cannot listen for signals: {0}")]
    Signal(std::io::Error),
    #[error("{E_SHUTDOWN_TIMEOUT}: the gateway did not stop in time")]
    ShutdownTimeout,
}

impl GatewayError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Store(err) => err.code(),
            Self::Adapter(err) => err.code,
            Self::Config(err) => err.code,
            Self::Task(_) => E_GATEWAY_TASK,
            Self::Signal(_) => E_GATEWAY_SIGNAL,
            Self::ShutdownTimeout => E_SHUTDOWN_TIMEOUT,
        }
    }
}

pub struct Gateway {
    config: Config,
    adapter: Arc<dyn MessengerAdapter>,
    store: Arc<Mutex<Store>>,
    omp: OmpManager,
}

impl Gateway {
    /// Opens the inbox and builds the omp manager with the lead guidance followed by the
    /// adapter's behavior hints (each replaced by its override file in the home,
    /// `E_HINTS_FILE` when unusable). A second gateway on the same home fails here with
    /// `E_ALREADY_RUNNING`. Nothing is started yet.
    pub fn open(config: &Config, adapter: Arc<dyn MessengerAdapter>) -> Result<Self, GatewayError> {
        let guidance = hints::load(&config.home, lead::GUIDANCE_NAME, lead::DEFAULT_GUIDANCE)?;
        let hints = hints::load(&config.home, adapter.platform(), adapter.behavior_hints())?;
        let store = Arc::new(Mutex::new(Store::open(&config.home.join(STORE_FILE))?));
        let omp = OmpManager::new(config.clone(), Arc::clone(&store))
            .with_hints(lead::system_prompt(&guidance, &hints));
        let tools =
            onboarding::host_tools(Arc::clone(&store), config.home.clone(), omp.turn_context());
        let omp = omp.with_host_tools(tools);
        Ok(Self {
            config: config.clone(),
            adapter,
            store,
            omp,
        })
    }

    /// Adjusts the omp manager (tests add child environment variables or a turn limit).
    pub fn with_omp(mut self, adjust: impl FnOnce(OmpManager) -> OmpManager) -> Self {
        self.omp = adjust(self.omp);
        self
    }

    /// `Some(conversation key)` while a lead turn that accepts a steer runs. Take it
    /// before [`Self::run`] consumes the gateway.
    pub fn running_turn(&self) -> watch::Receiver<Option<String>> {
        self.omp.running_turn()
    }

    /// Serves until `shutdown` resolves (or the adapter stops), then shuts down cleanly.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), GatewayError> {
        let Self {
            config,
            adapter,
            store,
            omp,
        } = self;
        let mut inbound = adapter.start().await?;
        let dispatcher = Arc::new(Dispatcher::new(&config, adapter, Arc::clone(&store), omp));
        let (events, queue) = mpsc::channel(EVENT_QUEUE);
        let mut dispatch = tokio::spawn(dispatcher.run(queue));
        tracing::info!("gateway started");

        tokio::pin!(shutdown);
        let ended_early = loop {
            tokio::select! {
                finished = &mut dispatch => break Some(finished),
                () = &mut shutdown => break None,
                event = inbound.recv() => match event {
                    Some(event) => {
                        if events.send(event).await.is_err() {
                            break None;
                        }
                    }
                    None => break None,
                },
            }
        };

        tracing::info!("gateway stopping");
        // Closing the queue ends the dispatcher: it finishes the event it is handling,
        // drops the in-flight turn and stops the lead and lane children.
        drop(events);
        drop(inbound);
        let finished = match ended_early {
            Some(finished) => finished,
            None => dispatch.await,
        };
        // A turn that was cut off is still `processing` and a stopped lane `running`:
        // make them `needs_check`.
        store
            .lock()
            .expect("store mutex poisoned")
            .recover_on_start()?;
        finished.map_err(GatewayError::Task)??;
        tracing::info!("gateway stopped");
        Ok(())
    }
}
