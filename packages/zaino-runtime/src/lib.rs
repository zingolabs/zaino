//! `zaino-runtime` — the orchestrator.
//!
//! Composes the finalised-state (`zaino-fs`) and non-finalised-state
//! (`zaino-nfs`) components into one running indexer, wires their loops
//! (bulk-build → tip-follow → freeze), composes their snapshots, and (will)
//! implement `zaino-service::IndexerService`. The async shell — `zaino-core`
//! and the component *logic* stay out of here.
//!
//! Scaffold: the two seams are shown — the **compose** seam
//! ([`Runtime::snapshot`] → [`RuntimeSnapshot`]) and the **loop-wiring** seam
//! ([`RuntimeBuilder::init`]).
#![forbid(unsafe_code)]

mod snapshot;

pub use snapshot::RuntimeSnapshot;

use std::sync::Arc;

use zaino_fs::FinalisedState;
use zaino_nfs::{NfsView, NonFinalisedState};

/// The running indexer: a finalised component + a non-finalised component.
pub struct Runtime<F, N> {
    fs: Arc<F>,
    nfs: Arc<N>,
}

impl<F, N> Runtime<F, N>
where
    F: FinalisedState + 'static,
    N: NonFinalisedState + 'static,
{
    /// **Compose seam.** Pin a snapshot spanning both tiers: the finalised state
    /// (shared handle) + a pinned NFS view, split at the current finalised
    /// watermark. Reads route FS (`≤ watermark`) vs NFS (`> watermark`).
    pub fn snapshot(&self) -> RuntimeSnapshot<F, N::Snapshot> {
        let watermark = self.fs.watermark();
        // A `Syncing` NFS contributes no recent coverage → recent reads become
        // `NotServiceable`, never a false `None`. The readiness is enforced by
        // the type, not by a runtime check we could forget.
        let nfs = match self.nfs.snapshot() {
            NfsView::Ready(s) => Some(s),
            NfsView::Syncing { .. } => None,
        };
        RuntimeSnapshot {
            fs: Arc::clone(&self.fs),
            nfs,
            watermark,
        }
    }
}

/// Builder + lifecycle for the runtime.
#[derive(Default)]
pub struct RuntimeBuilder {
    // TODO: source adapter, backend, index-set, executor (self-hosted vs ambient).
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// **Loop-wiring seam.** Boot the runtime: bulk-build the finalised state,
    /// then start the tip-follow loop and forward frozen blocks into the FS
    /// component.
    ///
    /// The wiring (commented below) needs the executor + a `zaino-source` bound
    /// on `S`; for now `init` just assembles the components.
    pub async fn init<F, N, S>(self, fs: F, nfs: N, _source: S) -> Result<Runtime<F, N>, RuntimeError>
    where
        F: FinalisedState + 'static,
        N: NonFinalisedState + 'static,
        S: Send + Sync + 'static,
    {
        let fs = Arc::new(fs);
        let nfs = Arc::new(nfs);

        // 1. Catch-up:  fs.bulk_build_to(tip - D, &source).await?;
        // 2. Tip-follow: spawn  nfs.follow(&source)  as a producer task.
        // 3. Freeze forward: drain  nfs.frozen()  → fs.freeze(block)  (a task).
        //    (tokio::spawn once the executor is wired.)

        Ok(Runtime { fs, nfs })
    }
}

/// Runtime lifecycle errors (placeholder).
#[derive(Debug)]
pub enum RuntimeError {
    /// Boot / bulk-build failure.
    Init(String),
}
