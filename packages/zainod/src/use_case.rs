//! Use cases as types: the one place a deployment's demand, routing and
//! materialisation are bound together.
//!
//! A serving profile (`LightServeService`, `NodeRpcService`) names what a use
//! case *demands*. A [`Routing`] names which provider answers each capability.
//! A [`Materialisation`] names which indexes the finalised store builds. Each
//! is a type, and each is checked by the compiler — but nothing stops a wiring
//! from pairing the light profile with the explorer's routing except common
//! sense, because the profile is a trait and the other two are free choices at
//! the wiring site.
//!
//! [`UseCase`] removes that seam. It is a marker type with the routing and the
//! materialisation as associated types, and [`Serves<U>`] carries the demand
//! as a bound the wiring can name generically. The composition root names a
//! use case, never a routing or an index set, and [`compose`] checks
//! `demand ⊆ supply` once, for any use case.
//!
//! ```text
//! wired(U) = Composed<StoreReader<B, U::Materialisation>, Nfs, Src, U::Routing>
//! ok(U)    ⟺ wired(U): Serves<U>
//! ```
//!
//! Config selects a use case from a closed set; it does not shape one. What a
//! use case *is* stays static.

use zaino_chain_head::ChainHeadBlockSource;
use zaino_indexer::CompactSource;
use zaino_indexes::materialisation::Materialisation;
use zaino_indexes::sets::current_zaino::CurrentZainoContext;
use zaino_indexes::sets::light_wallet::LightWallet as LightWalletIndexes;
use zaino_service::routing::{LightRouting, Routing};
use zaino_service::{ChainSegment, CompactBlockRead, LightServeService, TakeSnapshot};
use zaino_source::{
    GetAddressBalance, GetAddressDeltas, GetAddressTxids, GetAddressUtxos,
    GetMempoolCompactTransaction, GetMempoolSourceTip, GetMempoolTxids, GetRawMempoolTransaction,
    GetSubtreeRoots, GetTransaction, GetTreestate, SendRawTransaction,
};
use zaino_store::StoreReader;
use zaino_store_service::Composed;

/// What this daemon needs of any validator to boot at all, as one name: the
/// chain head's source port and the compact-block indexer's, both over the
/// **canonical** (resilient) ports — the daemon hands every consumer the same
/// `ValidatorClient` over one shared validator, so no consumer, and no bound
/// here, names a single-attempt port.
///
/// A new validator adapter implements the one-shot ports it can; the client
/// over it satisfies this exactly when those cover what the chain head and the
/// indexer ask, and a use case's `…Source` bundle names what else that use
/// case sends through.
pub trait DaemonSource: ChainHeadBlockSource + CompactSource + Clone {}
impl<S> DaemonSource for S where S: ChainHeadBlockSource + CompactSource + Clone {}

/// What the light-wallet use case requires of the validator, as one name: the
/// daemon floor plus every port its routing's remote placements and the
/// always-remote reads relay through.
///
/// Hand-kept beside the use case rather than derived, because Rust cannot
/// compute "the union of the source bounds of the impls this routing selects".
/// Safe to be wrong in one direction: a port missing here fails the profile
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

/// A deployment shape: its routing and its materialisation, bound together.
pub trait UseCase: 'static {
    /// The name config selects it by, and the serving component's name.
    const NAME: &'static str;
    /// Which provider answers each capability.
    type Routing: Routing;
    /// Which indexes the finalised store builds. Every materialisation today
    /// projects from the current-zaino provisioning context, which is what the
    /// indexer's compact-block provisioner produces.
    type Materialisation: Materialisation<Context = CurrentZainoContext>;
}

/// The demand of use case `U`, as a bound.
///
/// One blanket impl per use case forwards to its profile trait, so a wiring
/// can require `engine: Serves<U>` without naming the profile. Rust cannot
/// name a trait as an associated item; this is the indirection that stands in
/// for `type Profile`.
pub trait Serves<U: UseCase> {}

/// The engine a use case wires: the store over the use case's materialisation,
/// a head, a validator handle, under the use case's routing.
pub type Engine<U, B, Nfs, Src> =
    Composed<StoreReader<B, <U as UseCase>::Materialisation>, Nfs, Src, <U as UseCase>::Routing>;

/// Compose the engine for use case `U`, checking that it serves what `U`
/// demands.
///
/// The `where` clause is the whole point: a materialisation lacking an index
/// the profile's reads need, or a placement no provider can take, fails here —
/// at the one call site per use case — rather than at a request.
pub fn compose<U, B, Nfs, Src>(
    store: StoreReader<B, U::Materialisation>,
    head: Nfs,
    source: Src,
) -> Engine<U, B, Nfs, Src>
where
    U: UseCase,
    StoreReader<B, U::Materialisation>: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Nfs: TakeSnapshot<Snapshot: ChainSegment + CompactBlockRead>,
    Engine<U, B, Nfs, Src>: Serves<U>,
{
    Composed::new(store, head, source)
}

/// Lightwalletd-compatible serving: compact blocks composed locally from the
/// light-wallet index set, wallet-parsed reads passed through to the validator,
/// node reads withheld.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightWallet;

impl UseCase for LightWallet {
    const NAME: &'static str = "light-wallet";
    type Routing = LightRouting;
    type Materialisation = LightWalletIndexes;
}

impl<S: LightServeService> Serves<LightWallet> for S {}
