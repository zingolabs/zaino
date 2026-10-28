//! ChainIndex's side of the ChainStore boundary.
//!
//! How ChainIndex hands the finalised store a validator, and how the two
//! crates' heights and hashes line up.
//!
//! # This is a bridge, not a dependency between subsystems
//!
//! The chain store and the chain head know nothing about each other, and must
//! not: they are replaceable independently, and a dependency either way would
//! make one's rework touch the other. ChainIndex depends on both, because
//! ChainIndex is what composes them — and so it is ChainIndex that owns the
//! adapters. This module is the store's; [`super::chain_head`] is the head's.
//! They are deliberately identical in shape and share nothing.

use std::sync::Arc;

use zaino_chain_store::ChainStoreSource;

use crate::chain_index::{
    source::BlockchainSource, source_ports::ChainIndexSourcePorts,
    validator_source::ValidatorSource,
};

/// A source that can also answer the chain store's questions.
///
/// The store speaks the `zaino-source` ports directly, while ChainIndex still
/// consumes the wire-typed [`BlockchainSource`] scaffolding. This trait is how
/// the second hands over the first: an implementor exposes the underlying
/// validator, and the store is built on that rather than on the wrapper.
///
/// Kept off `BlockchainSource` because that port is frozen scaffolding
/// (docs/adr/zaino/0008) and shrinks as each subsystem moves onto the real ports.
pub trait WithChainStoreSource: BlockchainSource {
    /// The validator the store will build itself from.
    type Store: ChainStoreSource;

    /// The underlying validator, shared rather than cloned.
    ///
    /// Shared because a validator may own connections and a database handle
    /// that must not be duplicated — the same reason [`ChainStoreSource`] is
    /// not `Clone`.
    fn chain_store_source(&self) -> Arc<Self::Store>;
}

/// A `ValidatorSource` offers a chain-store source exactly when the validator
/// it wraps can answer the store's questions.
///
/// Both bounds are load-bearing and neither implies the other.
/// `ChainIndexSourcePorts` names what ChainIndex asks — which includes the
/// *raw* block ports, because it hands bytes to callers — and supplies
/// `GetTransaction`, which only the store's passthrough mode needs.
/// `ChainHeadBlockSource` supplies the parsed block reads. Naming the second
/// here is not a chain-head dependency: it is the shortest way to say "this
/// validator parses blocks", and the store's own requirement is stated by
/// [`ChainStoreSource`], which the compiler checks this satisfies.
impl<V> WithChainStoreSource for ValidatorSource<V>
where
    V: ChainIndexSourcePorts + zaino_chain_head::ChainHeadBlockSource,
{
    type Store = V;

    fn chain_store_source(&self) -> Arc<Self::Store> {
        self.validator()
    }
}

/// Opens the finalised store `config` names, built from `source`.
pub(super) async fn spawn<S: WithChainStoreSource>(
    config: &crate::config::ChainIndexConfig,
    source: &S,
) -> Result<
    zaino_chain_store_zainodb::store::FinalisedState<S::Store>,
    crate::error::FinalisedStateError,
> {
    zaino_chain_store_zainodb::store::FinalisedState::spawn(
        config.chain_store_config(),
        config.zainodb_config(),
        source.chain_store_source(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::WithChainStoreSource;

    /// A real validator satisfies the bridge.
    ///
    /// Nothing else checks it. The bridge is only named through generic bounds,
    /// so a requirement no adapter can meet compiles perfectly well here and
    /// fails wherever a concrete `ChainIndex` is first constructed.
    #[test]
    fn the_zebra_validator_source_offers_a_chain_store_source() {
        fn assert_satisfied<T: WithChainStoreSource>() {}
        assert_satisfied::<crate::chain_index::validator_source::ZebraValidatorSource>();
    }
}
