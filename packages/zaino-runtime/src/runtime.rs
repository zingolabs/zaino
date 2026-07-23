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
use crate::passthrough::Passthrough;
use crate::snapshot::RuntimeSnapshot;

/// The running indexer: a finalised component, a non-finalised component, a
/// passthrough handle to the validator, and config.
pub struct Runtime<F, N, P> {
    fs: Arc<F>,
    nfs: Arc<N>,
    passthrough: Arc<P>,
    cfg: Arc<RuntimeConfig>,
}

impl<F, N, P> Runtime<F, N, P>
where
    F: FinalisedState + 'static,
    N: NonFinalisedState + 'static,
    P: Passthrough + 'static,
{
    /// Pin a read-context spanning both tiers. A `Syncing` NFS contributes no
    /// recent coverage → recent reads become `NotServiceable`.
    pub fn snapshot(&self) -> RuntimeSnapshot<F, N::Snapshot, P> {
        let watermark = self.fs.watermark();
        let nfs = match self.nfs.snapshot() {
            NfsView::Ready(s) => Some(s),
            NfsView::Syncing { .. } => None,
        };
        RuntimeSnapshot {
            fs: Arc::clone(&self.fs),
            nfs,
            watermark,
            passthrough: Arc::clone(&self.passthrough),
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

    /// Boot: assemble the components + passthrough handle. Loop wiring
    /// (bulk-build → tip-follow → freeze-forward) needs the executor.
    pub async fn init<F, N, P>(
        self,
        fs: F,
        nfs: N,
        source: P,
    ) -> Result<Runtime<F, N, P>, RuntimeError>
    where
        F: FinalisedState + 'static,
        N: NonFinalisedState + 'static,
        P: Passthrough + 'static,
    {
        let fs = Arc::new(fs);
        let nfs = Arc::new(nfs);
        let passthrough = Arc::new(source);
        let cfg = Arc::new(self.cfg);

        // 1. fs.bulk_build_to(tip - D, &source).await?;
        // 2. spawn nfs.follow(&source);
        // 3. spawn: drain nfs.frozen() -> fs.freeze(block).

        Ok(Runtime {
            fs,
            nfs,
            passthrough,
            cfg,
        })
    }
}
