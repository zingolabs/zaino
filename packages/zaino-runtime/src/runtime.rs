//! The supervisor.
//!
//! Owns the components, runs their loops (sketch), aggregates serviceability,
//! holds config, and produces the pinned read-context. No query logic —
//! composition lives in [`crate::resolve`] / [`crate::snapshot`].

use std::sync::Arc;

use zaino_fs::FinalisedState;
use zaino_nfs::{NfsView, NonFinalisedState};

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::snapshot::RuntimeSnapshot;

/// The running indexer: a finalised component + a non-finalised component + config.
pub struct Runtime<F, N> {
    fs: Arc<F>,
    nfs: Arc<N>,
    cfg: Arc<RuntimeConfig>,
}

impl<F, N> Runtime<F, N>
where
    F: FinalisedState + 'static,
    N: NonFinalisedState + 'static,
{
    /// Pin a read-context spanning both tiers. A `Syncing` NFS contributes no
    /// recent coverage → recent reads become `NotServiceable`.
    pub fn snapshot(&self) -> RuntimeSnapshot<F, N::Snapshot> {
        let watermark = self.fs.watermark();
        let nfs = match self.nfs.snapshot() {
            NfsView::Ready(s) => Some(s),
            NfsView::Syncing { .. } => None,
        };
        RuntimeSnapshot {
            fs: Arc::clone(&self.fs),
            nfs,
            watermark,
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

    /// Boot: assemble the components. Loop wiring (bulk-build → tip-follow →
    /// freeze-forward) needs the executor + a `zaino-source` bound on `S`.
    pub async fn init<F, N, S>(
        self,
        fs: F,
        nfs: N,
        _source: S,
    ) -> Result<Runtime<F, N>, RuntimeError>
    where
        F: FinalisedState + 'static,
        N: NonFinalisedState + 'static,
        S: Send + Sync + 'static,
    {
        let fs = Arc::new(fs);
        let nfs = Arc::new(nfs);
        let cfg = Arc::new(self.cfg);

        // 1. fs.bulk_build_to(tip - D, &source).await?;
        // 2. spawn nfs.follow(&source);
        // 3. spawn: drain nfs.frozen() -> fs.freeze(block).

        Ok(Runtime { fs, nfs, cfg })
    }
}
