use anyhow::{Result, bail};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tempfile::tempdir;
use tokio::sync::oneshot;
use zigbee_bridge_rs::{session::Startup, store::Store};

/// Observe the actual file lock from the resource destructor.
struct Probe {
    path: PathBuf,
    released: Arc<AtomicBool>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        assert!(Store::open(&self.path).is_err());
        self.released.store(true, Ordering::SeqCst);
    }
}

#[test]
fn abandoned_startup_drops_acquired_handles_before_unlocking() -> Result<()> {
    let dir = tempdir()?;
    let released = Arc::new(AtomicBool::new(false));
    let probe = Probe {
        path: dir.path().into(),
        released: released.clone(),
    };
    let mut startup = Startup::new(Store::open(dir.path())?);
    startup.worker(move |_| async move {
        drop(probe);
        panic!("uncommitted startup ran")
    });
    drop(startup);
    assert!(released.load(Ordering::SeqCst));
    Store::open(dir.path())?;
    Ok(())
}

#[tokio::test]
async fn interrupted_wait_does_not_release_the_store_or_reopen_the_gate() -> Result<()> {
    let dir = tempdir()?;
    let mut startup = Startup::new(Store::open(dir.path())?);
    let (ready, started) = oneshot::channel();
    let (in_cleanup, stopping) = oneshot::channel();
    let (release, hold) = oneshot::channel();
    startup.worker(move |s| async move {
        ready.send(()).unwrap();
        s.cancelled().await;
        in_cleanup.send(()).unwrap();
        hold.await?;
        assert!(s.while_running(|| panic!("late completion")).is_none());
        Ok(())
    });
    let controller = startup.start();
    started.await?;
    tokio::select! {
        result = controller.shutdown() => panic!("premature receipt: {result:?}"),
        result = stopping => result?,
    }
    assert!(Store::open(dir.path()).is_err());
    release.send(()).unwrap();
    let (a, b) = tokio::join!(controller.shutdown(), controller.shutdown());
    a?;
    b?;
    controller.shutdown().await?;
    Store::open(dir.path())?;
    Ok(())
}

#[tokio::test]
async fn dropping_controller_leaves_cleanup_owned_until_worker_returns() -> Result<()> {
    let dir = tempdir()?;
    let mut startup = Startup::new(Store::open(dir.path())?);
    let (ready, started) = oneshot::channel();
    let (release, hold) = oneshot::channel();
    let (exiting, exited) = oneshot::channel();
    startup.worker(move |session| async move {
        ready.send(()).unwrap();
        session.cancelled().await;
        hold.await?;
        exiting.send(()).unwrap();
        Ok(())
    });
    let controller = startup.start();
    started.await?;
    let session = controller.session();
    drop(controller);
    assert!(session.while_running(|| true).is_none());
    assert!(Store::open(dir.path()).is_err());
    release.send(()).unwrap();
    exited.await?;
    // Even after the worker has signalled, only the supervisor can release the lock.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while Store::open(dir.path()).is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn panic_during_dispatch_revokes_the_session_and_drains_its_peers() -> Result<()> {
    let dir = tempdir()?;
    let mut startup = Startup::new(Store::open(dir.path())?);
    let (ready, started) = oneshot::channel();
    let (in_cleanup, stopping) = oneshot::channel();
    let (release, hold) = oneshot::channel();
    startup.worker(move |session| async move {
        ready.send(()).unwrap();
        session.cancelled().await;
        in_cleanup.send(()).unwrap();
        hold.await?;
        Ok(())
    });
    startup.worker(move |session| async move {
        started.await?;
        session.while_running(|| panic!("worker test panic"));
        Ok(())
    });
    let controller = startup.start();
    stopping.await?;
    assert!(Store::open(dir.path()).is_err());
    release.send(()).unwrap();
    assert!(
        controller
            .finished()
            .await
            .unwrap_err()
            .to_string()
            .contains("worker test panic")
    );
    Store::open(dir.path())?;
    Ok(())
}

#[tokio::test]
async fn worker_failure_is_reported_without_a_stop_request() -> Result<()> {
    let dir = tempdir()?;
    let mut startup = Startup::new(Store::open(dir.path())?);
    startup.worker(|_| async { bail!("transport closed") });
    let controller = startup.start();
    assert_eq!(
        controller.finished().await.unwrap_err().to_string(),
        "transport closed"
    );
    assert!(controller.session().while_running(|| true).is_none());
    Store::open(dir.path())?;
    Ok(())
}

#[test]
fn executor_loss_revokes_handles_and_releases_worker_resources() -> Result<()> {
    let dir = tempdir()?;
    let released = Arc::new(AtomicBool::new(false));
    let probe = Probe {
        path: dir.path().into(),
        released: released.clone(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let controller = runtime.block_on(async {
        let mut startup = Startup::new(Store::open(dir.path())?);
        let (ready, started) = oneshot::channel();
        startup.worker(move |_| async move {
            let _probe = probe;
            ready.send(()).unwrap();
            std::future::pending().await
        });
        let controller = startup.start();
        started.await?;
        anyhow::Ok(controller)
    })?;
    drop(runtime);
    assert!(controller.session().while_running(|| true).is_none());
    assert!(released.load(Ordering::SeqCst));
    Store::open(dir.path())?;
    let other = tokio::runtime::Builder::new_current_thread().build()?;
    assert!(other.block_on(controller.finished()).is_err());
    Ok(())
}
