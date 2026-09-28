//! Deployments: how a use case is served, as types.
//!
//! A use case (`zaino_service::use_cases`) names what a consumer demands. A
//! [`Deployment`] names how this runtime meets it: the [`Routing`] the engine
//! is composed under and the index set the finalised store builds. Each is a
//! type, and each is checked by the compiler — but nothing stops a wiring from
//! pairing the light-wallet demand with the explorer's routing except common
//! sense, because the demand is a trait and the other two are free choices at
//! the wiring site. A deployment removes that seam: it is a marker type with
//! the three as associated types, and [`compose`] checks `demand ⊆ supply`
//! once, for any deployment, through the use case's [`Serves`] bound.
//!
//! ```text
//! wired(D) = Engine<StoreReader<B, D::Indexes>, Nfs, Src, D::Routing>
//! ok(D)    ⟺ wired(D): Serves<D::UseCase>
//! ```
//!
//! One use case, several deployments: the light-wallet demand served with
//! address history relayed to the validator today, and with a local
//! transparent index later, are two deployments of one use case. Config
//! selects a deployment from a closed set; it does not shape one.
//!
//! Each deployment is one file under this module holding the marker, its
//! [`Deployment`] impl, its [`RuntimePlan`](crate::RuntimePlan) impl and the
//! validator bundle it requires. A deployment that outgrows a file — one that
//! brings a provider with a dependency nobody else wants — moves to a crate of
//! its own without changing anything above it, because the seam is these
//! traits, not a crate boundary.

mod light_wallet_passthrough;

pub use light_wallet_passthrough::{LightWalletPassthrough, LightWalletSource};

use zaino_chain_head::ChainHeadBlockSource;
use zaino_core::routing::Routing;
use zaino_core::Engine;
use zaino_indexer::CompactSource;
use zaino_indexes::index_set::{Builds, IndexSet};
use zaino_indexes::indexes::headers::HeadersIndex;
use zaino_indexes::sets::current_zaino::CurrentZainoContext;
use zaino_service::use_cases::{Serves, UseCase};
use zaino_service::{ChainSegment, CompactBlockRead, TakeSnapshot};
use zaino_store::StoreReader;

/// What the indexed assembly needs of any validator to boot at all, as one
/// name: the chain head's source port and the compact-block indexer's, both
/// over the **canonical** (resilient) ports — the runtime hands every consumer
/// the same `ValidatorClient` over one shared validator, so no consumer, and
/// no bound here, names a single-attempt port.
///
/// A new validator adapter implements the one-shot ports it can; the client
/// over it satisfies this exactly when those cover what the chain head and the
/// indexer ask, and a deployment's `…Source` bundle names what else it sends
/// through.
pub trait IndexedSource: ChainHeadBlockSource + CompactSource + Clone {}
impl<S> IndexedSource for S where S: ChainHeadBlockSource + CompactSource + Clone {}

/// One way of serving a use case: the use case, the routing the engine is
/// composed under, and the index set the finalised store builds.
pub trait Deployment: Send + Sync + 'static {
    /// The demand this deployment meets.
    type UseCase: UseCase;
    /// Which provider answers each capability.
    type Routing: Routing;
    /// Which indexes the finalised store builds. Every index set today
    /// projects from the current-zaino provisioning context, which is what the
    /// indexer's compact-block provisioner produces, and every one builds the
    /// headers index: the store pins its tip from it and checks its watermark
    /// against it.
    type Indexes: IndexSet<Context = CurrentZainoContext> + Builds<HeadersIndex>;
}

/// The engine a deployment wires: the store over its index set, a head, a
/// validator handle, under its routing.
pub type DeploymentEngine<D, B, Nfs, Src> =
    Engine<StoreReader<B, <D as Deployment>::Indexes>, Nfs, Src, <D as Deployment>::Routing>;

/// Compose the engine for deployment `D`, checking that it serves what `D`'s
/// use case demands.
///
/// The `where` clause is the whole point: an index set lacking an index the
/// demand's reads need, or a placement no provider can take, fails here — at
/// the one call site per deployment — rather than at a request.
pub fn compose<D, B, Nfs, Src>(
    store: StoreReader<B, D::Indexes>,
    head: Nfs,
    source: Src,
) -> DeploymentEngine<D, B, Nfs, Src>
where
    D: Deployment,
    StoreReader<B, D::Indexes>: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    DeploymentEngine<D, B, Nfs, Src>: Serves<D::UseCase>,
{
    Engine::new(store, head, source)
}
