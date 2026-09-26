//! Reaching ChainView from this crate's source vocabulary.
//!
//! The same shape as [`WithChainHeadSource`](super::chain_head::WithChainHeadSource)
//! and [`WithChainStoreSource`](super::chain_store::WithChainStoreSource): a
//! consumer-owned bound saying which questions *this* consumer needs a
//! validator to answer. `zaino-source` should not have to know who its
//! consumers are, so the list lives with the consumer.
//!
//! # Scaffolding
//!
//! This exists so `ChainIndex` can read through `zaino-chain` while the crates
//! above it still speak the old vocabulary. It goes when `ChainIndex` does.

use std::sync::Arc;

use zaino_chain::ChainViewSource;

use super::source::BlockchainSource;
use super::source_ports::ChainIndexSourcePorts;
use super::validator_source::ValidatorSource;

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
    use super::WithChainViewSource;

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
        assert_view::<
            zaino_chain::ChainViewComposer<
                zaino_chain_store_zainodb::store::FinalisedState<
                    zaino_source_zebra::ZebraValidator,
                >,
                zaino_chain_head_service::ChainHeadSubscriber,
                zaino_source_zebra::ZebraValidator,
            >,
        >();
    }
}
