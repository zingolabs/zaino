//! The chain index's legacy type vocabulary, and its conversions to and from
//! the domain types ChainView and the mempool speak.
//!
//! The legacy shapes live in `zaino-chain-store-zainodb` and are re-exported
//! because the public `ChainIndex` traits are written against them. Every
//! conversion between them and the domain is here, so both services convert
//! the same way.
//!
//! The wire conversions stay in [`super::wire_types`].

use std::collections::HashSet;
use std::num::NonZeroU128;

use zaino_chain::{ChainBlock, ChainTxPosition, SpendStatus, TransactionLocations};
use zaino_primitives::types as domain;

use crate::chain_index::source::BlockchainSourceError;
use crate::error::ChainIndexError;

pub use zaino_chain_store_zainodb::types::*;

pub(crate) fn domain_hash(hash: BlockHash) -> domain::BlockHash {
    domain::BlockHash::from(hash.0)
}

pub(crate) fn local_hash(hash: domain::BlockHash) -> BlockHash {
    BlockHash(hash.into())
}

/// `None` beyond the protocol maximum, where no block can be.
pub(crate) fn domain_height(height: Height) -> Option<domain::Height> {
    domain::Height::try_from(height.0).ok()
}

pub(crate) fn local_height(height: domain::Height) -> Height {
    Height(u32::from(height))
}

pub(crate) fn domain_txid(txid: &TransactionHash) -> domain::TransactionId {
    domain::TransactionId::from(txid.0)
}

pub(crate) fn local_txid(txid: domain::TransactionId) -> TransactionHash {
    TransactionHash(txid.into())
}

pub(crate) fn domain_scope(scope: ChainScope) -> zaino_chain::ChainScope {
    match scope {
        ChainScope::Finalised => zaino_chain::ChainScope::Finalised,
        ChainScope::FullChain => zaino_chain::ChainScope::FullChain,
    }
}

pub(crate) fn block_index(block: domain::BlockRef) -> BlockIndex {
    BlockIndex {
        height: local_height(block.height),
        hash: local_hash(block.hash),
    }
}

/// The transaction that spent an outpoint, or `None` if none in scope did.
///
/// A spend whose spender is unknown is an error: `None` would claim the output
/// unspent.
pub(crate) fn spender(status: SpendStatus) -> Result<Option<TransactionHash>, ChainIndexError> {
    match status {
        SpendStatus::Unspent => Ok(None),
        SpendStatus::SpentBy(txid) => Ok(Some(local_txid(txid))),
        other => Err(ChainIndexError::internal(format!(
            "outpoint is spent but its spender cannot be named: {other:?}"
        ))),
    }
}

/// Where a transaction was mined, in the legacy location shapes.
pub(crate) fn chain_locations(
    locations: TransactionLocations,
) -> (Option<BestChainLocation>, HashSet<NonBestChainLocation>) {
    let at = |position: ChainTxPosition| {
        (
            local_hash(position.block.hash),
            local_height(position.block.height),
        )
    };
    (
        locations.best_chain.map(|position| {
            let (hash, height) = at(position);
            BestChainLocation::Block(hash, height)
        }),
        locations
            .non_best_chain
            .into_iter()
            .map(|position| {
                let (hash, height) = at(position);
                NonBestChainLocation::Block(hash, height)
            })
            .collect(),
    )
}

/// A ChainView block, in the legacy shape.
///
/// The store's conversion takes a chainwork, so a placeholder stands in for it
/// and is replaced by the view's own: the real value, or `None` where the view
/// cannot know it.
pub(crate) fn indexed_block(block: ChainBlock) -> Result<IndexedBlock, ChainIndexError> {
    let chainwork = block.chainwork;
    let stored = zaino_chain_store::StoredBlock {
        header: block.header,
        transactions: block.transactions,
        tree_roots: block.tree_roots,
        chainwork: chainwork.unwrap_or(domain::AbsoluteChainWork::new(NonZeroU128::MIN)),
    };
    Ok(
        zaino_chain_store_zainodb::adapter::indexed_block_from_stored(&stored)?
            .map_chainwork(|_| chainwork),
    )
}

/// The consensus branch a transaction at `height` is validated under.
pub(crate) fn branch_id(
    network: &zebra_chain::parameters::Network,
    height: domain::Height,
) -> Option<u32> {
    zebra_chain::parameters::ConsensusBranchId::current(
        network,
        zebra_chain::block::Height(u32::from(height)),
    )
    .map(u32::from)
}

/// The addresses of a balance or UTXO request, rejected as the legacy source
/// rejected them.
pub(crate) fn balance_request_addresses(
    request: &zebra_rpc::client::GetAddressBalanceRequest,
) -> Result<Vec<domain::TransparentAddress>, ChainIndexError> {
    use zebra_rpc::methods::ValidateAddresses as _;

    let invalid = |error: &dyn std::fmt::Display| {
        ChainIndexError::backing_validator(BlockchainSourceError::Unrecoverable(format!(
            "invalid address: {error}"
        )))
    };
    request
        .valid_addresses()
        .map_err(|error| invalid(&error))?
        .into_iter()
        .map(|address| {
            domain::TransparentAddress::try_new(address.to_string())
                .map_err(|error| invalid(&error))
        })
        .collect()
}

/// Encoded addresses the validator is asked about directly, rejected as it
/// rejects them.
pub(crate) fn encoded_addresses(
    addresses: impl IntoIterator<Item = String>,
) -> Result<Vec<domain::TransparentAddress>, ChainIndexError> {
    addresses
        .into_iter()
        .map(|address| {
            domain::TransparentAddress::try_new(address)
                .map_err(ChainIndexError::validator_rejected)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain_ref(height: u32, tag: u8) -> domain::BlockRef {
        domain::BlockRef {
            hash: domain::BlockHash::from([tag; 32]),
            height: domain::Height::try_from(height).expect("test height is in range"),
        }
    }

    #[test]
    fn identifiers_survive_the_round_trip() {
        let hash = BlockHash([7; 32]);
        assert_eq!(local_hash(domain_hash(hash)), hash);

        let txid = TransactionHash([9; 32]);
        assert_eq!(local_txid(domain_txid(&txid)), txid);

        let height = Height(1_234);
        assert_eq!(
            domain_height(height).map(local_height),
            Some(height),
            "a valid height survives"
        );
    }

    #[test]
    fn a_height_beyond_the_protocol_maximum_names_no_block() {
        assert_eq!(domain_height(Height(u32::MAX)), None);
    }

    #[test]
    fn a_block_ref_becomes_a_block_index() {
        let index = block_index(domain_ref(5, 3));
        assert_eq!(index.height, Height(5));
        assert_eq!(index.hash, BlockHash([3; 32]));
    }

    #[test]
    fn a_spend_with_no_nameable_spender_is_an_error_not_unspent() {
        assert_eq!(spender(SpendStatus::Unspent).expect("unspent"), None);
        assert_eq!(
            spender(SpendStatus::SpentBy(domain::TransactionId::from([4; 32]))).expect("spent"),
            Some(TransactionHash([4; 32]))
        );
        assert!(spender(SpendStatus::SpentSpenderUnknown).is_err());
    }

    #[test]
    fn locations_keep_best_and_competing_blocks_apart() {
        let mut locations = TransactionLocations::default();
        locations.best_chain = Some(ChainTxPosition {
            block: domain_ref(10, 1),
            tx_index: 0,
        });
        locations.non_best_chain = vec![ChainTxPosition {
            block: domain_ref(10, 2),
            tx_index: 1,
        }];

        let (best, others) = chain_locations(locations);
        assert_eq!(
            best,
            Some(BestChainLocation::Block(BlockHash([1; 32]), Height(10)))
        );
        assert_eq!(
            others,
            HashSet::from([NonBestChainLocation::Block(BlockHash([2; 32]), Height(10))])
        );
    }

    #[test]
    fn an_unparseable_address_is_rejected() {
        let error = encoded_addresses(["not an address".to_string()])
            .expect_err("an unparseable address is invalid");
        assert!(error.to_string().contains("validator rejected the query"));
    }

    /// A vector's `u64` tree size, as the domain carries it.
    fn vector_tree_size(size: u64) -> domain::TreeSize {
        domain::TreeSize::try_from(size).expect("vector tree sizes fit u32")
    }

    /// The store's domain-block conversion agrees with the `zebra_chain` path
    /// the golden vectors pin, except for chain work, which it is not given.
    #[test]
    fn domain_block_conversion_agrees_with_the_zebra_path() {
        use crate::chain_index::tests::vectors::{indexed_block_chain, load_test_vectors};

        let vectors = load_test_vectors().expect("test vectors load");
        for (vector, expected) in vectors
            .blocks
            .iter()
            .zip(indexed_block_chain(&vectors.blocks))
        {
            let block = zaino_convert_zebra::block_from_zebra(
                &vector.zebra_block,
                domain::ChainMetadata::new(
                    vector_tree_size(vector.sapling_tree_size),
                    vector_tree_size(vector.orchard_tree_size),
                    domain::TreeSize::ZERO,
                ),
            )
            .expect("vector block converts to the domain shape");
            let tree_roots = domain::TreeRoots {
                sapling: Some(domain::TreeRootInfo {
                    root: <[u8; 32]>::from(vector.sapling_root).into(),
                    size: vector_tree_size(vector.sapling_tree_size),
                }),
                orchard: Some(domain::TreeRootInfo {
                    root: <[u8; 32]>::from(vector.orchard_root).into(),
                    size: vector_tree_size(vector.orchard_tree_size),
                }),
                ironwood: None,
            };

            let actual: IndexedBlock =
                zaino_chain_store_zainodb::conversion::indexed_block(&block, &tree_roots, None)
                    .expect("conversion succeeds");

            assert_eq!(actual.context.index, expected.context.index, "block index");
            assert_eq!(actual.context.parent_hash, expected.context.parent_hash);
            assert_eq!(actual.context.chainwork, None);
            assert_eq!(actual.data, expected.data, "block header data");
            assert_eq!(actual.commitment_tree_data, expected.commitment_tree_data);
            assert_eq!(actual.transactions.len(), expected.transactions.len());
            for (actual_tx, expected_tx) in actual.transactions.iter().zip(expected.transactions())
            {
                assert_eq!(actual_tx.transparent(), expected_tx.transparent());
                assert_eq!(actual_tx.index(), expected_tx.index(), "transaction index");
                assert_eq!(actual_tx.txid(), expected_tx.txid(), "txid");
                assert_eq!(
                    actual_tx.balances(),
                    expected_tx.balances(),
                    "pool balances"
                );
            }
        }
    }

    /// Every coinbase carries the null prevout the stored form keeps, exactly
    /// once, and no other transaction carries one.
    #[test]
    fn the_coinbase_null_prevout_is_present_exactly_once() {
        use crate::chain_index::tests::vectors::{indexed_block_chain, load_test_vectors};

        let vectors = load_test_vectors().expect("test vectors load");
        for expected in indexed_block_chain(&vectors.blocks) {
            for transaction in expected.transactions() {
                let nulls = transaction
                    .transparent()
                    .inputs()
                    .iter()
                    .filter(|input| **input == TxInCompact::null_prevout())
                    .count();
                assert_eq!(nulls, usize::from(transaction.index() == 0));
            }
        }
    }
}
