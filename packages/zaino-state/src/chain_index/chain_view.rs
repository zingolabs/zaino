//! Reaching ChainView from this crate's source vocabulary, and composing it.
//!
//! The same shape as [`WithChainHeadSource`](super::chain_head::WithChainHeadSource)
//! and [`WithChainStoreSource`](super::chain_store::WithChainStoreSource): a
//! consumer-owned bound saying which questions *this* consumer needs a
//! validator to answer. `zaino-source` should not have to know who its
//! consumers are, so the list lives with the consumer.

use std::sync::Arc;

use zaino_chain::{ChainViewComposer, ChainViewConfig, ChainViewSource, ComposerSnapshot};
use zaino_chain_head_service::{ChainHeadSubscriber, MapBackedSnapshot};
use zaino_chain_store_zainodb::store::{reader::DbReader, FinalisedState};

use super::chain_store::WithChainStoreSource;
use super::source::BlockchainSource;
use super::source_ports::ChainIndexSourcePorts;
use super::validator_source::ValidatorSource;

/// The chain view `ChainIndex` reads through.
pub(crate) type Composer<S> = ChainViewComposer<
    FinalisedState<<S as WithChainStoreSource>::Store>,
    ChainHeadSubscriber,
    <S as WithChainViewSource>::View,
>;

/// The snapshot every `ChainIndex` read answers from.
pub(crate) type ChainIndexSnapshot<S> = ComposerSnapshot<
    DbReader<<S as WithChainStoreSource>::Store>,
    MapBackedSnapshot,
    <S as WithChainViewSource>::View,
>;

/// A chain view snapshot's tip, under the name this crate uses: it is the best
/// chain's tip, not the only one.
pub(crate) trait BestTip {
    fn best_tip(&self) -> zaino_primitives::types::BlockRef;
}

impl<T: zaino_chain::ChainViewSnapshot> BestTip for T {
    fn best_tip(&self) -> zaino_primitives::types::BlockRef {
        self.tip()
    }
}

/// A chain view over the finalised store and the chain head, with the
/// validator filling what neither holds.
pub(super) fn compose<S: WithChainStoreSource + WithChainViewSource>(
    store: FinalisedState<S::Store>,
    head: ChainHeadSubscriber,
    source: &S,
) -> Arc<Composer<S>> {
    Arc::new(ChainViewComposer::new(
        store,
        head,
        source.chain_view_source(),
        ChainViewConfig::default(),
    ))
}

/// A source that can also answer a chain view's passthrough questions.
pub trait WithChainViewSource: BlockchainSource {
    /// The validator, as ChainView needs it.
    type View: ChainViewSource;

    /// A handle onto it.
    fn chain_view_source(&self) -> Arc<Self::View>;
}

/// A `ValidatorSource` offers a ChainView source exactly when the validator it
/// wraps can answer ChainView's questions.
///
/// The second bound is not redundant with the first, for the same reason as
/// ChainHead's: `ChainIndexSourcePorts` names what `ChainIndex` itself asks,
/// while `ChainViewSource` names what a chain view passes through. Each bound
/// describes one consumer's needs rather than restating another's.
impl<V> WithChainViewSource for ValidatorSource<V>
where
    V: ChainIndexSourcePorts + ChainViewSource,
{
    type View = V;

    fn chain_view_source(&self) -> Arc<Self::View> {
        self.validator()
    }
}

#[cfg(test)]
mod tests {
    use super::{Composer, WithChainViewSource};

    /// The real source can drive a chain view, and the composed view is a
    /// `ChainView`.
    ///
    /// Nothing else checks that the three providers `ChainIndex` already holds
    /// actually satisfy `zaino-chain`'s bounds together — a port list that no
    /// real source could satisfy would compile perfectly well and fail at
    /// wiring time. The same assertion guards `ChainStoreSource` and
    /// `ChainHeadBlockSource` in their own crates.
    #[test]
    fn the_real_source_composes_into_a_chain_view() {
        fn assert_source<S: WithChainViewSource>() {}
        assert_source::<crate::chain_index::validator_source::ZebraValidatorSource>();

        fn assert_view<V: zaino_chain::ChainView>() {}
        assert_view::<Composer<crate::chain_index::validator_source::ZebraValidatorSource>>();
    }
}
