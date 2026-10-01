//! `zaino-store-service` — the finalised store, as a supervised component.
//!
//! The runtime half of the `zaino-store` tandem: it wraps the passive
//! [`StoreReader`](zaino_store::StoreReader) so the Orchestra can own, boot it
//! in dependency order — after the indexer it reads behind — and supervise it.
//! This mirrors `zaino-chain-head-service` for the non-finalised tier;
//! `zaino-store` itself stays a reader with no component dependency.
//!
//! Unlike the indexer (which runs the sync engine) or a server (which binds a
//! socket), the reader is **passive**: it has no run-loop, so its lifecycle is
//! `Offline → Spawning → Ready` — ready as soon as the backend is open. It never
//! fails on its own, so it never escalates. Boot ordering (the indexer reaching
//! `Ready` first) is the Orchestra's job, not a wait here.

use std::sync::Arc;

use tokio::sync::watch;
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, Managed, StatusSource, StatusWatch,
};
use zaino_persistence::Backend;
use zaino_store::StoreReader;

/// A [`StoreReader`] presented to the runtime as an owned component.
pub struct StoreComponent<B, M> {
    name: ComponentName,
    reader: Arc<StoreReader<B, M>>,
    status: watch::Sender<ComponentStatus>,
}

impl<B, M> Clone for StoreComponent<B, M> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            reader: Arc::clone(&self.reader),
            status: self.status.clone(),
        }
    }
}

impl<B, M> StoreComponent<B, M> {
    /// A store component named `name` serving `reader`, initially `Offline`.
    pub fn new(name: ComponentName, reader: StoreReader<B, M>) -> Self {
        let (status, _) = watch::channel(ComponentStatus::new(
            name,
            Lifecycle::Offline,
            Health::Offline,
        ));
        Self {
            name,
            reader: Arc::new(reader),
            status,
        }
    }

    /// The reader this component supervises — how the engine / serving layer
    /// takes snapshots against the store.
    pub fn reader(&self) -> Arc<StoreReader<B, M>> {
        Arc::clone(&self.reader)
    }
}

impl<B: Send + Sync + 'static, M: Send + Sync + 'static> StatusSource for StoreComponent<B, M> {
    fn status(&self) -> ComponentStatus {
        self.status.borrow().clone()
    }
}

impl<B: Send + Sync + 'static, M: Send + Sync + 'static> StatusWatch for StoreComponent<B, M> {
    fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
        self.status.subscribe()
    }
}

impl<B: Backend + 'static, M: Send + Sync + 'static> Managed for StoreComponent<B, M> {
    // The reader is passive and the backend is already open (the runtime
    // opened it, and repaired the watermark, before building the reader), so
    // bringup cannot fail here and there is no run task whose `Err` could
    // surface.
    type Error = std::convert::Infallible;

    async fn spawn(&self) -> Result<(), Self::Error> {
        // Passive, so no task: transition straight through `Spawning` to `Ready`.
        // (`Offline → Ready` is not a legal jump; the reader really is spawning,
        // it just has nothing long-running to start.)
        self.status
            .send_modify(|s| s.lifecycle = Lifecycle::Spawning);
        self.status.send_modify(|s| {
            s.lifecycle = Lifecycle::Ready;
            s.health = Health::Healthy;
        });
        Ok(())
    }

    async fn restart(&self) -> Result<(), Self::Error> {
        self.stop().await?;
        self.spawn().await
    }

    async fn stop(&self) -> Result<(), Self::Error> {
        self.status
            .send_modify(|s| s.lifecycle = Lifecycle::Closing);
        self.status.send_modify(|s| {
            s.lifecycle = Lifecycle::Offline;
            s.health = Health::Offline;
        });
        Ok(())
    }
}
