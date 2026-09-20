//! The finalised store reader as a supervised component. EXPLORATORY.
//!
//! [`StoreComponent`] presents a [`StoreReader`](crate::StoreReader) to the
//! runtime as an **owned** ([`Managed`]) component, so the Orchestra can boot it
//! in dependency order — after the indexer it reads behind — and supervise it.
//!
//! Unlike the indexer (which runs the sync engine) or a server (which binds a
//! socket), the reader is **passive**: it has no run-loop, so its lifecycle is
//! `Offline → Spawning → Ready` — ready as soon as the backend is open. It never
//! fails on its own, so it never escalates. Boot ordering (the indexer reaching
//! `Ready` first) is the Orchestra's job, not a wait here. EXPLORATORY STUB —
//! see the crate docs.

use std::sync::Arc;

use tokio::sync::watch;
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, Managed, StatusSource, StatusWatch,
};
use zaino_persistence::Backend;

use crate::StoreReader;

/// A [`StoreReader`] presented to the runtime as an owned component.
pub struct StoreComponent<B> {
    name: ComponentName,
    reader: Arc<StoreReader<B>>,
    status: watch::Sender<ComponentStatus>,
}

impl<B> Clone for StoreComponent<B> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            reader: Arc::clone(&self.reader),
            status: self.status.clone(),
        }
    }
}

impl<B> StoreComponent<B> {
    /// A store component named `name` serving `reader`, initially `Offline`.
    pub fn new(name: ComponentName, reader: StoreReader<B>) -> Self {
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
    pub fn reader(&self) -> Arc<StoreReader<B>> {
        Arc::clone(&self.reader)
    }
}

impl<B: Send + Sync + 'static> StatusSource for StoreComponent<B> {
    fn status(&self) -> ComponentStatus {
        self.status.borrow().clone()
    }
}

impl<B: Send + Sync + 'static> StatusWatch for StoreComponent<B> {
    fn subscribe(&self) -> watch::Receiver<ComponentStatus> {
        self.status.subscribe()
    }
}

impl<B: Backend + 'static> Managed for StoreComponent<B> {
    // The reader is passive: opening a KV read handle does not fail in this
    // stub, so bringup cannot fail synchronously and there is no run task whose
    // `Err` could surface. A real store that can fail to open would carry a
    // typed error here instead of `Infallible`.
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
