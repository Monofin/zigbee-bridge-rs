//! Ownership and terminal receipts for one running bridge.
use crate::store::Store;
use anyhow::{Result, anyhow};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::{sync::watch, task::JoinSet};

#[derive(Clone)]
pub struct Session {
    running: Arc<Mutex<bool>>,
    stop: watch::Sender<bool>,
}

impl Session {
    /// Serialize a short synchronous action with revocation. The action must
    /// neither block on another worker nor re-enter the session gate.
    pub fn while_running<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let running = self.running.lock().ok()?;
        running.then(action)
    }

    pub fn request_stop(&self) {
        let mut running = self.running.lock().unwrap_or_else(|p| p.into_inner());
        if *running {
            *running = false;
            self.stop.send_replace(true);
        }
    }

    pub async fn cancelled(&self) {
        let mut signal = self.stop.subscribe();
        let _ = signal.wait_for(|stopping| *stopping).await;
    }
}

// Also revoke escaped session handles when the executor drops the supervisor.
struct RevokeOnDrop(Session);
impl Drop for RevokeOnDrop {
    fn drop(&mut self) {
        self.0.request_stop();
    }
}

type Worker = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

// Field order matters on executor cancellation. The worker drops its handles
// before its share of the exclusive store lock, even if the supervisor is gone.
struct OwnedWorker {
    body: Worker,
    _store: Arc<Store>,
}

impl OwnedWorker {
    async fn run(mut self) -> Result<()> {
        self.body.as_mut().await
    }
}

pub struct Startup {
    workers: Vec<Worker>,
    session: Session,
    store: Store,
}

impl Startup {
    pub fn new(store: Store) -> Self {
        Self {
            workers: Vec::new(),
            session: Session {
                running: Arc::new(Mutex::new(true)),
                stop: watch::channel(false).0,
            },
            store,
        }
    }

    #[cfg(unix)]
    pub(crate) fn session(&self) -> Session {
        self.session.clone()
    }

    pub fn worker<F, Fut>(&mut self, build: F)
    where
        F: FnOnce(Session) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let session = self.session.clone();
        self.workers.push(Box::pin(async move {
            if session.while_running(|| ()).is_none() {
                return Ok(());
            }
            build(session).await
        }));
    }

    pub fn start(self) -> Controller {
        let session = self.session.clone();
        let (finished, receipt) = watch::channel(None);
        let revoker = RevokeOnDrop(session.clone());
        let store = Arc::new(self.store);
        let workers: Vec<_> = self
            .workers
            .into_iter()
            .map(|body| OwnedWorker {
                body,
                _store: store.clone(),
            })
            .collect();
        tokio::spawn(async move {
            let _revoker = revoker;
            let mut tasks = JoinSet::new();
            for worker in workers {
                tasks.spawn(worker.run());
            }
            let mut error = None;
            if tasks.is_empty() {
                self.session.cancelled().await;
            } else {
                tokio::select! {
                    biased;
                    _ = self.session.cancelled() => {},
                    first = tasks.join_next() => {
                        let unexpected = self.session.while_running(|| ()).is_some();
                        self.session.request_stop();
                        error = match first.expect("registered worker") {
                            Ok(Ok(())) if unexpected => Some(anyhow!("worker exited while serving")),
                            Ok(result) => result.err(),
                            Err(join) => Some(join.into()),
                        };
                    }
                }
            }
            while let Some(joined) = tasks.join_next().await {
                if let Err(e) = joined.map_err(anyhow::Error::from).and_then(|r| r) {
                    error.get_or_insert(e);
                }
            }
            drop(store);
            finished.send_replace(Some(error.map_or(Ok(()), |e| Err(format!("{e:#}")))));
        });
        Controller { session, receipt }
    }
}

pub struct Controller {
    session: Session,
    receipt: watch::Receiver<Option<Result<(), String>>>,
}
impl Controller {
    pub fn session(&self) -> Session {
        self.session.clone()
    }

    /// Multiple observers may wait, time out or resume waiting independently.
    pub async fn shutdown(&self) -> Result<()> {
        self.session.request_stop();
        self.finished().await
    }

    pub async fn finished(&self) -> Result<()> {
        let mut receiver = self.receipt.clone();
        let receipt = receiver
            .wait_for(Option::is_some)
            .await
            .map_err(|_| anyhow!("supervisor exited without a shutdown receipt"))?;
        receipt
            .as_ref()
            .expect("terminal receipt")
            .clone()
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.session.request_stop();
    }
}
