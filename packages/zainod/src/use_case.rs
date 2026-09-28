//! Deployments: how this daemon serves a use case, as types.
//!
//! A use case (`zaino_service::use_cases`) names what a consumer demands. A
//! deployment names how this daemon meets it: the [`Routing`] the engine is
//! composed under and the index set the finalised store builds. Each is a
//! type, and each is checked by the compiler — but nothing stops a wiring from
//! pairing the light-wallet demand with the explorer's routing except common
//! sense, because the demand is a trait and the other two are free choices at
//! the wiring site.
//!
//! [`Deployment`] removes that seam. It is a marker type with the use case,
//! the routing and the index set as associated types, and [`compose`] checks
//! `demand ⊆ supply` once, for any deployment, through the use case's
//! [`Serves`] bound.
//!
//! ```text
//! wired(D) = Engine<StoreReader<B, D::Indexes>, Nfs, Src, D::Routing>
//! ok(D)    ⟺ wired(D): Serves<D::UseCase>
//! ```
//!
//! Config selects a deployment from a closed set; it does not shape one. What
//! a deployment *is* stays static.

use zaino_chain_head::ChainHeadBlockSource;
use zaino_core::routing::{LightRouting, Routing};
use zaino_core::Engine;
use zaino_indexer::CompactSource;
use zaino_indexes::indexes::headers::HeadersIndex;
use zaino_indexes::materialisation::{Builds, Materialisation};
use zaino_indexes::sets::current_zaino::CurrentZainoContext;
use zaino_indexes::sets::light_wallet::LightWallet as LightWalletIndexes;
use zaino_service::use_cases::{LightWallet, Serves, UseCase};
use zaino_service::{ChainSegment, CompactBlockRead, TakeSnapshot};
use zaino_source::{
    GetAddressBalance, GetAddressDeltas, GetAddressTxids, GetAddressUtxos,
    GetMempoolCompactTransaction, GetMempoolSourceTip, GetMempoolTxids, GetRawMempoolTransaction,
    GetSubtreeRoots, GetTransaction, GetTreestate, SendRawTransaction,
};
use zaino_store::StoreReader;

/// What this daemon needs of any validator to boot at all, as one name: the
/// chain head's source port and the compact-block indexer's, both over the
/// **canonical** (resilient) ports — the daemon hands every consumer the same
/// `ValidatorClient` over one shared validator, so no consumer, and no bound
/// here, names a single-attempt port.
///
/// A new validator adapter implements the one-shot ports it can; the client
/// over it satisfies this exactly when those cover what the chain head and the
/// indexer ask, and a deployment's `…Source` bundle names what else it sends
/// through.
pub trait DaemonSource: ChainHeadBlockSource + CompactSource + Clone {}
impl<S> DaemonSource for S where S: ChainHeadBlockSource + CompactSource + Clone {}

/// What the light-wallet passthrough deployment requires of the validator, as
/// one name: the daemon floor plus every port its routing's passthrough
/// placements and the always-passthrough reads relay through.
///
/// Hand-kept beside the deployment rather than derived, because Rust cannot
/// compute "the union of the source bounds of the impls this routing selects".
/// Safe to be wrong in one direction: a port missing here fails the demand
/// bound at the wiring, naming it.
pub trait LightWalletSource:
    DaemonSource
    + SendRawTransaction
    + GetMempoolTxids
    + GetMempoolSourceTip
    + GetRawMempoolTransaction
    + GetMempoolCompactTransaction
    + GetTransaction
    + GetTreestate
    + GetSubtreeRoots
    + GetAddressBalance
    + GetAddressUtxos
    + GetAddressTxids
    + GetAddressDeltas
{
}
impl<S> LightWalletSource for S where
    S: DaemonSource
        + SendRawTransaction
        + GetMempoolTxids
        + GetMempoolSourceTip
        + GetRawMempoolTransaction
        + GetMempoolCompactTransaction
        + GetTransaction
        + GetTreestate
        + GetSubtreeRoots
        + GetAddressBalance
        + GetAddressUtxos
        + GetAddressTxids
        + GetAddressDeltas
{
}

/// One way of serving a use case: the use case, the routing the engine is
/// composed under, and the index set the finalised store builds.
pub trait Deployment: 'static {
    /// The demand this deployment meets.
    type UseCase: UseCase;
    /// Which provider answers each capability.
    type Routing: Routing;
    /// Which indexes the finalised store builds. Every index set today
    /// projects from the current-zaino provisioning context, which is what the
    /// indexer's compact-block provisioner produces, and every one builds the
    /// headers index: the store pins its tip from it and checks its watermark
    /// against it.
    type Indexes: Materialisation<Context = CurrentZainoContext> + Builds<HeadersIndex>;
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

/// The light-wallet use case served with compact blocks composed locally from
/// the light-wallet index set and everything the wallet parses itself
/// relayed to the validator; node reads withheld.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightWalletPassthrough;

impl Deployment for LightWalletPassthrough {
    type UseCase = LightWallet;
    type Routing = LightRouting;
    type Indexes = LightWalletIndexes;
}
