//! The supervisor.
//!
//! Owns the components, runs their loops (sketch), aggregates serviceability,
//! holds config + the passthrough handle, and produces the pinned read-context.
//! No query logic — composition lives in [`crate::resolve`] / [`crate::snapshot`].

use std::sync::Arc;

use zaino_fs::FinalisedState;
use zaino_nfs::{NfsView, NonFinalisedState};

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::passthrough::PassthroughSource;
use crate::snapshot::RuntimeSnapshot;

/// The running indexer: a finalised component, a non-finalised component, and
/// the validator source — the one dependency the components build/follow from
/// *and* the runtime reads through (passthrough) — plus config.
pub struct Runtime<F, N, Src> {
    fs: Arc<F>,
    nfs: Arc<N>,
    source: Arc<Src>,
    cfg: Arc<RuntimeConfig>,
}

impl<F, N, Src> Runtime<F, N, Src>
where
    F: FinalisedState + 'static,
    N: NonFinalisedState + 'static,
    Src: PassthroughSource + 'static,
{
    /// Pin a read-context spanning both tiers. A `Syncing` NFS contributes no
    /// recent coverage → recent reads become `NotServiceable`.
    pub fn snapshot(&self) -> RuntimeSnapshot<F, N::Snapshot, Src> {
        let watermark = self.fs.watermark();
        let nfs = match self.nfs.snapshot() {
            NfsView::Ready(s) => Some(s),
            NfsView::Syncing { .. } => None,
        };
        RuntimeSnapshot {
            fs: Arc::clone(&self.fs),
            nfs,
            watermark,
            source: Arc::clone(&self.source),
            cfg: Arc::clone(&self.cfg),
        }
    }
}

/// Builder + lifecycle for the runtime.
#[derive(Default)]
pub struct RuntimeBuilder {
    cfg: RuntimeConfig,
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the deployment config.
    pub fn config(mut self, cfg: RuntimeConfig) -> Self {
        self.cfg = cfg;
        self
    }

    /// Boot: assemble the components over the shared validator `source`. That
    /// one dependency is used three ways — `fs` builds from it, `nfs` follows
    /// it, and the read path passes through to it. Loop wiring (bulk-build →
    /// tip-follow → freeze-forward) needs the executor.
    pub async fn init<F, N, Src>(
        self,
        fs: F,
        nfs: N,
        source: Src,
    ) -> Result<Runtime<F, N, Src>, RuntimeError>
    where
        F: FinalisedState + 'static,
        N: NonFinalisedState + 'static,
        Src: PassthroughSource + 'static,
    {
        let fs = Arc::new(fs);
        let nfs = Arc::new(nfs);
        let source = Arc::new(source);
        let cfg = Arc::new(self.cfg);

        // 1. fs.bulk_build_to(tip - D, &*source).await?;
        // 2. spawn nfs.follow(&*source);
        // 3. spawn: drain nfs.frozen() -> fs.freeze(block).

        Ok(Runtime {
            fs,
            nfs,
            source,
            cfg,
        })
    }
}
