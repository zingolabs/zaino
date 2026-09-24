//! Finalised-state backend
//!
//! The finalised state has one backend, the LMDB-backed [`v1::DbV1`]. This file holds the
//! lifecycle scaffolding that backend shares with any future schema, and the helper that opens
//! its tables.
//!
//! # On-disk directory layout
//!
//! The v1 database lives in `<network>/v1/` under the configured path, where `<network>` is
//! `mainnet`, `testnet` or `regtest`.
//!
//! # Development: adding new indices/queries
//!
//! Implement new indices in `v1`, expose them through an extension trait in `capability.rs`, and
//! add a `DbReader` method for them. An optional index is gated by a cargo feature.
//!
//! A new index adds a table, so it changes the computed schema hash, and every existing
//! database rebuilds on its next start.

pub(crate) mod v1;

use crate::error::StoreError;
use crate::support::SendFut;
use zaino_status::{NamedAtomicStatus, StatusType};

use lmdb::{Database, DatabaseFlags, Environment};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    task::JoinHandle,
    time::{interval, sleep, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Lifecycle scaffolding shared by every `DbVx` finalised-state backend.
///
/// Implementors expose the four shared struct fields via required getters;
/// provided methods cover the duplicated `status()`, `wait_until_ready()`,
/// `shutdown()`, `clean_trailing()`, and the background task's per-iteration
/// `zaino_db_handler_sleep()`.
///
/// Note: This trait ties any DB version that uses it to Lmdb.
/// In the future we may want to support alternative DB backends.
/// When this happens, we will have to lean away from this trait to some extent.
pub(super) trait LmdbLifecycle: Sync {
    fn env(&self) -> &Arc<Environment>;
    fn db_handler_slot(&self) -> &Mutex<Option<JoinHandle<()>>>;
    fn cancel_token(&self) -> &CancellationToken;
    fn status_atomic(&self) -> &NamedAtomicStatus;

    fn status(&self) -> StatusType {
        self.status_atomic().load()
    }

    fn wait_until_ready(&self) -> impl SendFut<()> {
        async move {
            let mut ticker = interval(Duration::from_millis(100));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if self.status_atomic().load() == StatusType::Ready {
                    break;
                }
            }
        }
    }

    fn clean_trailing(&self) -> impl SendFut<Result<(), StoreError>> {
        async move {
            let txn = self.env().begin_ro_txn()?;
            drop(txn);
            Ok(())
        }
    }

    fn zaino_db_handler_sleep(&self, maintenance: &mut tokio::time::Interval) -> impl SendFut<()> {
        async move {
            tokio::select! {
                _ = sleep(Duration::from_secs(5)) => {},
                _ = maintenance.tick() => {
                    if let Err(e) = self.clean_trailing().await {
                        warn!(%e, "clean_trailing failed");
                    }
                }
                _ = self.cancel_token().cancelled() => {},
            }
        }
    }

    fn shutdown(&self) -> impl SendFut<Result<(), StoreError>> {
        async move {
            self.status_atomic().store(StatusType::Closing);
            self.cancel_token().cancel();

            let taken = self
                .db_handler_slot()
                .lock()
                .expect("db_handler mutex poisoned")
                .take();
            if let Some(mut handle) = taken {
                let timeout = sleep(Duration::from_secs(5));
                tokio::pin!(timeout);

                tokio::select! {
                    res = &mut handle => {
                        match res {
                            Ok(_) => {}
                            Err(e) if e.is_cancelled() => {}
                            Err(e) => warn!(?e, "background task ended with error"),
                        }
                    }
                    _ = &mut timeout => {
                        warn!("background task didn't exit in time – aborting");
                        handle.abort();
                    }
                }
            }

            let _ = self.clean_trailing().await;
            if let Err(e) = self.env().sync(true) {
                warn!(%e, "LMDB fsync before close failed");
            }
            Ok(())
        }
    }
}

/// Open an LMDB database if present, otherwise create it.
pub(super) async fn open_or_create_db(
    env: &Environment,
    name: &str,
    flags: DatabaseFlags,
) -> Result<Database, StoreError> {
    match env.open_db(Some(name)) {
        Ok(db) => Ok(db),
        Err(lmdb::Error::NotFound) => env
            .create_db(Some(name), flags)
            .map_err(StoreError::LmdbError),
        Err(e) => Err(StoreError::LmdbError(e)),
    }
}

#[cfg(test)]
mod shutdown {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{sync::Barrier, time::timeout};

    struct FakeDb {
        env: Arc<Environment>,
        db_handler: Mutex<Option<JoinHandle<()>>>,
        cancel_token: CancellationToken,
        status: NamedAtomicStatus,
    }

    impl LmdbLifecycle for FakeDb {
        fn env(&self) -> &Arc<Environment> {
            &self.env
        }
        fn db_handler_slot(&self) -> &Mutex<Option<JoinHandle<()>>> {
            &self.db_handler
        }
        fn cancel_token(&self) -> &CancellationToken {
            &self.cancel_token
        }
        fn status_atomic(&self) -> &NamedAtomicStatus {
            &self.status
        }
    }

    /// Regression for #1033 — every task awaiting cancellation must observe shutdown,
    /// not just one. Originally written against the `Notify::notify_one` implementation
    /// (which strands N-1 waiters); now passes against `CancellationToken::cancel`,
    /// which wakes all current waiters and persists state for late subscribers.
    #[tokio::test]
    async fn wakes_every_shutdown_waiter() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Arc::new(
            lmdb::Environment::new()
                .set_map_size(1 << 20)
                .open(tmp.path())
                .unwrap(),
        );
        let db = Arc::new(FakeDb {
            env,
            db_handler: Mutex::new(None),
            cancel_token: CancellationToken::new(),
            status: NamedAtomicStatus::new("test", StatusType::Ready),
        });

        const N: usize = 3;
        let woke = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(N + 1));

        let mut waiters = Vec::with_capacity(N);
        for _ in 0..N {
            let token = db.cancel_token.clone();
            let woke = Arc::clone(&woke);
            let barrier = Arc::clone(&barrier);
            waiters.push(tokio::spawn(async move {
                barrier.wait().await;
                token.cancelled().await;
                woke.fetch_add(1, Ordering::Relaxed);
            }));
        }
        barrier.wait().await;

        LmdbLifecycle::shutdown(db.as_ref()).await.unwrap();

        for (i, w) in waiters.into_iter().enumerate() {
            timeout(Duration::from_millis(200), w)
                .await
                .unwrap_or_else(|_| panic!("waiter {i} stranded: cancel_token woke only a subset"))
                .unwrap();
        }
        assert_eq!(woke.load(Ordering::Relaxed), N);
    }
}
