//! Library entry of the gateway: wires the store, the omp manager, a messenger adapter
//! and the dispatcher, and shuts them down in order.
//!
//! The adapter is injected, so tests run the whole gateway with `FakeAdapter` while
//! `moraebot run` passes the configured messenger's adapter.
//!
//! Shutdown order: stop taking events from the adapter, let the event being handled
//! finish its store write, drop the in-flight turn, stop the lanes, kill and reap the
//! lead and every lane child, then mark the interrupted rows and lanes `needs_check` so
//! nothing is re-prompted, then stop the memory observer and wait for the memory writer.
//!
//! The memory observer ([`Observer`]) runs as its own task beside the dispatcher, not as one
//! of its supervised tasks: a fault in memory code never stops the gateway.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinError;

use crate::adapter::{AdapterError, MessengerAdapter};
use crate::config::{Config, ConfigError};
use crate::dispatch::Dispatcher;
use crate::hints;
use crate::jev::{self, Jev};
use crate::lead;
use crate::memory::{self, MemoryError, MemoryExit, MemoryHandle, MemoryStore, Redactor};
use crate::observer::Observer;
use crate::omp::{OmpManager, lock};
use crate::omp_overlay::{self, OverlayCheck};
use crate::onboarding;
use crate::store::{Store, StoreError};

/// File name of the inbox database inside the home folder.
pub const STORE_FILE: &str = "inbox.sqlite";
/// The dispatcher task, or one of its background tasks, panicked, ended or was cancelled.
pub const E_GATEWAY_TASK: &str = "E_GATEWAY_TASK";
/// The process could not listen for termination signals.
pub const E_GATEWAY_SIGNAL: &str = "E_GATEWAY_SIGNAL";
/// The gateway did not finish shutting down within its time bound.
pub const E_SHUTDOWN_TIMEOUT: &str = "E_SHUTDOWN_TIMEOUT";

/// Capacity of the queue between the adapter and the dispatcher.
const EVENT_QUEUE: usize = 64;
/// How long shutdown waits for the memory worker to finish its last commit and release the lock.
const MEMORY_STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long shutdown waits for the memory observer's pass before it aborts it.
const OBSERVER_STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Adapter(#[from] AdapterError),
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Memory(#[from] MemoryError),
    #[error("{E_GATEWAY_TASK}: dispatcher task failed: {0}")]
    Task(JoinError),
    #[error("{E_GATEWAY_TASK}: the {0} task stopped")]
    TaskEnded(&'static str),
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
            Self::Memory(err) => err.code,
            Self::Task(_) | Self::TaskEnded(_) => E_GATEWAY_TASK,
            Self::Signal(_) => E_GATEWAY_SIGNAL,
            Self::ShutdownTimeout => E_SHUTDOWN_TIMEOUT,
        }
    }
}

pub struct Gateway {
    config: Config,
    adapter: Arc<dyn MessengerAdapter>,
    store: Arc<Mutex<Store>>,
    memory: MemoryHandle,
    memory_exit: MemoryExit,
    omp: OmpManager,
    observer: Observer,
    /// Notified by the store after every commit that queued a memory job.
    memory_wake: Arc<Notify>,
}

impl Gateway {
    /// Opens the inbox and builds the omp manager with the lead guidance followed by the
    /// adapter's behavior hints (each replaced by its override file in the home,
    /// `E_HINTS_FILE` when unusable). A second gateway on the same home fails here with
    /// `E_ALREADY_RUNNING`. It also opens the memory store under `<home>/memory` (refusing an
    /// old 0.2.0 layout with `E_MEMORY_OLD_LAYOUT`) and starts its writer thread, which
    /// serves the owner's memory commands, and writes the omp overlay the lead and lanes run
    /// with (`E_OMP_OVERLAY` when it cannot); nothing else is started yet. The memory store
    /// and the observer share one redactor that also hides the jev key.
    pub fn open(config: &Config, adapter: Arc<dyn MessengerAdapter>) -> Result<Self, GatewayError> {
        omp_overlay::write_overlay(&config.home)?;
        let guidance = hints::load(&config.home, lead::GUIDANCE_NAME, lead::DEFAULT_GUIDANCE)?;
        let hints = hints::load(&config.home, adapter.platform(), adapter.behavior_hints())?;
        let store = Arc::new(Mutex::new(Store::open(&config.home.join(STORE_FILE))?));
        let memory_wake = Arc::new(Notify::new());
        let wake = Arc::clone(&memory_wake);
        lock(&store).set_memory_wake(Arc::new(move || wake.notify_one()));
        let jev_key = config.memory.jev.api_key();
        let redactor = Redactor::from_config(config)
            .with_own(jev_key.iter().map(|key| key.expose().to_owned()));
        let (memory, memory_exit) = MemoryHandle::spawn_with_exit(MemoryStore::open(
            &config.home,
            redactor.clone(),
            memory::system_clock(),
        )?)?;
        let observer = Observer::new(
            config,
            Arc::clone(&store),
            memory.clone(),
            Arc::new(Jev::new(config.memory.jev.clone(), jev::system_now())),
            redactor,
            jev::system_now(),
        );
        let omp = OmpManager::new(config.clone(), Arc::clone(&store))
            .with_hints(lead::system_prompt(&guidance, &hints))
            .with_memory(memory.clone());
        let tools =
            onboarding::host_tools(Arc::clone(&store), config.home.clone(), omp.turn_context());
        let omp = omp.with_host_tools(tools);
        Ok(Self {
            config: config.clone(),
            adapter,
            store,
            memory,
            memory_exit,
            omp,
            observer,
            memory_wake,
        })
    }

    /// Hands over what the start check found about omp: its version goes with every memory
    /// job the observer settles (`unknown` without this call).
    pub fn with_omp_check(mut self, check: &OverlayCheck) -> Self {
        self.observer = self.observer.with_omp_version(&check.version);
        self
    }

    /// The count of memory jobs the observer has settled. Take it before [`Self::run`].
    pub fn memory_progress(&self) -> watch::Receiver<u64> {
        self.observer.progress()
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
            memory,
            memory_exit,
            omp,
            observer,
            memory_wake,
        } = self;
        let mut inbound = adapter.start().await?;
        let (stop_observer, observer_stopped) = oneshot::channel::<()>();
        let mut observer = tokio::spawn(Arc::new(observer).run(memory_wake, async {
            let _ = observer_stopped.await;
        }));
        let dispatcher = Arc::new(
            Dispatcher::new(&config, adapter, Arc::clone(&store), omp).with_memory(memory),
        );
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
        let recovered = lock(&store).recover_on_start();
        let _ = stop_observer.send(());
        if tokio::time::timeout(OBSERVER_STOP_LIMIT, &mut observer)
            .await
            .is_err()
        {
            tracing::warn!("memory observer did not stop in time; aborted");
            observer.abort();
            let _ = observer.await;
        }
        recovered?;
        // The dispatcher and the observer held the last memory handles; wait for the worker to release the
        // memory lock so the home is free when `run` returns.
        let released = tokio::task::spawn_blocking(move || memory_exit.wait(MEMORY_STOP_LIMIT))
            .await
            .unwrap_or(false);
        if !released {
            tracing::warn!("memory worker still running at shutdown");
        }
        finished.map_err(GatewayError::Task)??;
        tracing::info!("gateway stopped");
        Ok(())
    }
}
