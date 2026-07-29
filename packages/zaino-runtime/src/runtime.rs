//! The supervisor.
//!
//! Owns the components, runs their loops (sketch), aggregates serviceability,
//! holds config + the passthrough handle, and produces the pinned read-context.
//! No query logic — composition lives in [`crate::resolve`] / [`crate::snapshot`].

use std::sync::Arc;

use zaino_core::{Capability, ServiceabilityManifest};
use zaino_fs::{AddressIndex, FinalisedSpine};
use zaino_nfs::{NfsAddressFacts, NfsSpendFacts, NfsSpine, NfsView, NonFinalisedState};
use zaino_service::Serviceable;

use crate::config::{CapabilitySet, RuntimeConfig};
use crate::error::RuntimeError;
use crate::passthrough::PassthroughSource;
use crate::serviceability::{self, State};
use crate::snapshot::RuntimeSnapshot;

/// The running indexer: a finalised component, a non-finalised component, and
/// the validator source — the one dependency the components build/follow from
/// *and* the runtime reads through (passthrough) — plus config and the set of
/// optional capabilities this deployment serves.
pub struct Runtime<F, N, Src> {
    fs: Arc<F>,
    nfs: Arc<N>,
    source: Arc<Src>,
    cfg: Arc<RuntimeConfig>,
    /// Optional index-backed capabilities opted into at assembly. Populated only
    /// through the assembler's type-gated `serving_*` methods, so it can't name
    /// a capability the components can't back. Read by both the manifest and the
    /// reads → advertised and answerable stay in lockstep.
    served: Arc<CapabilitySet>,
}

impl<F, N, Src> Runtime<F, N, Src>
where
    F: FinalisedSpine + 'static,
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
            served: Arc::clone(&self.served),
        }
    }
}

/// Serviceability is a **control** capability (live, not pinned): it reports
/// what's answerable *now*. It reads the finalised watermark and the NFS
/// readiness, then delegates the projection to [`serviceability`].
impl<F, N, Src> Serviceable for Runtime<F, N, Src>
where
    F: FinalisedSpine + 'static,
    N: NonFinalisedState + 'static,
    Src: Send + Sync,
{
    fn serviceability(&self) -> ServiceabilityManifest {
        let nfs_tip = match self.nfs.snapshot() {
            NfsView::Ready(s) => Some(s.tip().height),
            NfsView::Syncing { .. } => None,
        };
        serviceability::manifest(
            &self.cfg,
            &self.served,
            &State {
                finalized_tip: self.fs.watermark(),
                nfs_tip,
            },
        )
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

    /// Take the components over the shared validator `source` (used three ways —
    /// `fs` builds from it, `nfs` follows it, the read path passes through to
    /// it) and return an [`Assembler`]. The always-available capabilities (route
    /// reads on the spine, passthrough per config) are served implicitly; the
    /// optional index-backed ones are opted into with the assembler's type-gated
    /// `serving_*` methods before [`Assembler::finish`].
    pub fn assemble<F, N, Src>(self, fs: F, nfs: N, source: Src) -> Assembler<F, N, Src>
    where
        F: FinalisedSpine + 'static,
        N: NonFinalisedState + 'static,
        Src: PassthroughSource + 'static,
    {
        Assembler {
            fs,
            nfs,
            source,
            cfg: self.cfg,
            served: CapabilitySet::default(),
        }
    }
}

/// Accumulates the served-capability set under type gates, then boots the
/// runtime. Each `serving_*` method is bounded on the component traits that back
/// its capability, so a deployment can only declare what it can actually answer.
pub struct Assembler<F, N, Src> {
    fs: F,
    nfs: N,
    source: Src,
    cfg: RuntimeConfig,
    served: CapabilitySet,
}

impl<F, N, Src> Assembler<F, N, Src>
where
    F: FinalisedSpine + 'static,
    N: NonFinalisedState + 'static,
    Src: PassthroughSource + 'static,
{
    /// Serve transparent-address history — a merge over both tiers. Type-gated:
    /// only compiles when the finalised state builds the address index **and**
    /// the recent window can re-derive address facts.
    pub fn serving_address_history(mut self) -> Self
    where
        F: AddressIndex,
        N::Snapshot: NfsAddressFacts + NfsSpendFacts,
    {
        self.served.insert(Capability::AddressHistory);
        self
    }

    /// Boot: wire the lifecycle loop and produce the runtime. Loop wiring
    /// (bulk-build → tip-follow → freeze-forward) needs the executor.
    pub async fn finish(self) -> Result<Runtime<F, N, Src>, RuntimeError> {
        let source = Arc::new(self.source);

        // 1. fs.bulk_build_to(tip - D, &*source).await?;
        // 2. spawn nfs.follow(&*source);
        // 3. spawn: drain nfs.frozen() -> fs.freeze(block).

        Ok(Runtime {
            fs: Arc::new(self.fs),
            nfs: Arc::new(self.nfs),
            source,
            cfg: Arc::new(self.cfg),
            served: Arc::new(self.served),
        })
    }
}
