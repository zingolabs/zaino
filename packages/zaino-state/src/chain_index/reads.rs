//! `ChainIndex`, answered by the chain view and the mempool.

use std::collections::HashSet;

use futures::{Stream, StreamExt as _};
use zaino_chain::{
    AddressRead as _, BlockId, BlockRead as _, ForkReconcile as _, Locator, SpendRead as _,
    TransactionRead as _, TreestateRead as _,
};
use zaino_mempool::ports::TipAwareMempool as _;
use zaino_primitives::types::TransactionLocation;
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

use super::chain_head::WithChainHeadSource;
use super::chain_store::WithChainStoreSource;
use super::chain_view::{BestTip as _, ChainIndexSnapshot, WithChainViewSource};
use super::node_backed::NodeBackedChainIndexSubscriber;
use super::source::{BlockchainSource, BlockchainSourceError, PoolTreestate};
use super::types::{
    self, balance_request_addresses, block_index, chain_locations, domain_hash, domain_height,
    domain_scope, domain_txid, encoded_addresses, indexed_block, local_hash, local_height,
    local_txid, BestChainLocation, BlockIndex, NonBestChainLocation,
};
use super::{ChainIndex, ShieldedPool};
use crate::error::{ChainIndexError, ChainIndexErrorKind};
use crate::IndexedBlock;

/// A chain view's chunked range, one item at a time.
pub(super) fn flatten<T>(
    chunks: impl Stream<Item = zaino_chain::Result<Vec<T>>>,
) -> impl Stream<Item = Result<T, ChainIndexError>> {
    chunks.flat_map(|chunk| {
        futures::stream::iter(match chunk {
            Ok(items) => items.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(error) => vec![Err(ChainIndexError::from(error))],
        })
    })
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > NodeBackedChainIndexSubscriber<Source>
{
    /// The transaction as `snapshot`'s chain holds it, with the branch id of
    /// the block that mines it. `None` when no block of the snapshot holds it.
    async fn mined_transaction(
        &self,
        snapshot: &ChainIndexSnapshot<Source>,
        txid: zaino_primitives::types::TransactionId,
    ) -> Result<Option<(Vec<u8>, Option<u32>)>, ChainIndexError> {
        let Some(transaction) = snapshot.raw_transaction(txid).await? else {
            return Ok(None);
        };
        let height = match transaction.location {
            TransactionLocation::BestChain(height) => height,
            TransactionLocation::NonBestChain => {
                let locations = snapshot.transaction_locations(txid).await?;
                match locations
                    .best_chain
                    .or_else(|| locations.non_best_chain.first().copied())
                {
                    Some(position) => position.block.height,
                    None => return Ok(None),
                }
            }
            TransactionLocation::Mempool => return Ok(None),
        };
        Ok(Some((
            transaction.bytes,
            types::branch_id(&self.network, height),
        )))
    }
}

/// A request height, rejected as the legacy source rejected it.
fn request_height(height: u32) -> Result<zaino_primitives::types::Height, ChainIndexError> {
    zaino_primitives::types::Height::try_from(height).map_err(|error| {
        ChainIndexError::backing_validator(BlockchainSourceError::Unrecoverable(error.to_string()))
    })
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > ChainIndex for NodeBackedChainIndexSubscriber<Source>
{
    type Snapshot = ChainIndexSnapshot<Source>;
    type Error = ChainIndexError;

    fn snapshot_nonfinalized_state(&self) -> Self::Snapshot {
        self.composer.snapshot()
    }

    async fn get_block_height(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, Self::Error> {
        Ok(snapshot
            .block_height(domain_hash(hash))
            .await?
            .map(local_height))
    }

    async fn get_block_hash(
        &self,
        snapshot: &Self::Snapshot,
        height: types::Height,
    ) -> Result<Option<types::BlockHash>, Self::Error> {
        let Some(height) = domain_height(height) else {
            return Ok(None);
        };
        Ok(snapshot.block_hash(height).await?.map(local_hash))
    }

    async fn get_indexed_block_by_hash(
        &self,
        snapshot: &Self::Snapshot,
        target_hash: &types::BlockHash,
    ) -> Result<Option<IndexedBlock>, Self::Error> {
        snapshot
            .block(BlockId::Hash(domain_hash(*target_hash)))
            .await?
            .map(indexed_block)
            .transpose()
    }

    async fn get_indexed_block_by_height(
        &self,
        snapshot: &Self::Snapshot,
        target_height: &types::Height,
    ) -> Result<Option<IndexedBlock>, Self::Error> {
        let Some(height) = domain_height(*target_height) else {
            return Ok(None);
        };
        snapshot
            .block(BlockId::Height(height))
            .await?
            .map(indexed_block)
            .transpose()
    }

    fn get_block_range(
        &self,
        snapshot: &Self::Snapshot,
        start: types::Height,
        end: Option<types::Height>,
    ) -> Option<impl Stream<Item = Result<Vec<u8>, Self::Error>>> {
        let tip = snapshot.best_tip().height;
        let at_or_below_tip = |height: types::Height| {
            domain_height(height)
                .filter(|height| *height <= tip)
                .ok_or_else(|| {
                    ChainIndexError::invalid_argument(format!(
                        "height {height} is above the chain tip {tip}"
                    ))
                })
        };
        let range = at_or_below_tip(start)
            .and_then(|start| Ok((start, end.map_or(Ok(tip), at_or_below_tip)?)));
        Some(match range {
            Ok((start, end)) => {
                futures::future::Either::Left(flatten(snapshot.stream_raw_blocks(start, end)))
            }
            Err(error) => {
                futures::future::Either::Right(futures::stream::once(async { Err(error) }))
            }
        })
    }

    async fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<Option<(Vec<u8>, Option<u32>)>, Self::Error> {
        let txid = domain_txid(txid);

        // The mempool, but only when it is coherent with this snapshot: an
        // unmined transaction's branch id comes from the height it would be
        // mined at, which a stale snapshot would get wrong.
        let coherent = self.coherence.coherent_snapshot();
        match Self::coherent_epoch(&coherent, snapshot) {
            Some(epoch) => {
                if let Some(entry) = coherent.get(&txid) {
                    let branch_id = epoch
                        .best_tip
                        .height
                        .checked_add(1)
                        .and_then(|next| types::branch_id(&self.network, next));
                    return Ok(Some((entry.wire_bytes().to_vec(), branch_id)));
                }
            }
            None if self.mempool.contains_txid(&txid) => {
                // The view still lists it, but a block that mines it may have
                // reached this snapshot since the view was last blessed. The
                // snapshot's chain decides; only an unmined transaction waits
                // for a coherent view.
                return match self.mined_transaction(snapshot, txid).await? {
                    Some(mined) => Ok(Some(mined)),
                    None => Err(ChainIndexError::unavailable(
                        "mempool is not coherent with the requested snapshot; retry with a fresh snapshot",
                    )),
                };
            }
            None => {}
        }

        self.mined_transaction(snapshot, txid).await
    }

    async fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<(Option<BestChainLocation>, HashSet<NonBestChainLocation>), ChainIndexError> {
        let txid = domain_txid(txid);
        let (mut best, mut non_best) = chain_locations(snapshot.transaction_locations(txid).await?);

        if self.mempool.contains_txid(&txid) {
            let coherent = self.coherence.coherent_snapshot();
            if Self::coherent_epoch(&coherent, snapshot).is_some() {
                if best.is_some() {
                    return Err(ChainIndexError {
                        kind: ChainIndexErrorKind::InvalidSnapshot,
                        message:
                            "Best chain and up-to-date mempool both contain the same transaction"
                                .to_string(),
                        source: None,
                    });
                }
                best = Some(BestChainLocation::Mempool(
                    local_height(snapshot.best_tip().height) + 1,
                ));
            } else {
                // The height it would be mined at under the mempool's tip, if
                // this index still holds that block.
                let target_height = match coherent.valid_for {
                    Some(epoch) => self
                        .composer
                        .snapshot()
                        .block_height(epoch.best_tip.hash)
                        .await?
                        .map(|height| local_height(height) + 1),
                    None => None,
                };
                non_best.insert(NonBestChainLocation::Mempool(target_height));
            }
        }
        Ok((best, non_best))
    }

    async fn get_mempool_txids(&self) -> Result<Vec<types::TransactionHash>, Self::Error> {
        Ok(self
            .mempool
            .get_txids()
            .iter()
            .copied()
            .map(local_txid)
            .collect())
    }

    async fn get_mempool_transactions(
        &self,
        exclude_list: Vec<Vec<u8>>,
    ) -> Result<Vec<std::sync::Arc<zaino_mempool::MempoolEntry>>, Self::Error> {
        // Validated rather than clamped: silently truncating would serve
        // transactions the caller believes it excluded.
        let suffixes = self
            .mempool
            .validate_exclude_suffixes(&exclude_list)
            .map_err(|e| ChainIndexError::invalid_argument(e.to_string()))?;
        Ok(self.mempool.get_filtered_entries(&suffixes))
    }

    fn get_mempool_stream(
        &self,
        snapshot: Option<&Self::Snapshot>,
    ) -> Option<impl Stream<Item = Result<bytes::Bytes, Self::Error>>> {
        let expected = match snapshot {
            None => None,
            Some(snapshot) => Some(Self::coherent_epoch(
                &self.coherence.coherent_snapshot(),
                snapshot,
            )?),
        };
        let stream = self
            .coherence
            .stream_transactions_until_tip_change(expected)?;
        Some(stream.map(|item| {
            // A lag is not a normal end: reporting it as one would have the
            // client believe it had received the whole mempool.
            item.map_err(|e| {
                ChainIndexError::unavailable(format!("mempool stream ended early: {e}"))
            })
        }))
    }

    async fn best_chaintip(&self, snapshot: &Self::Snapshot) -> Result<BlockIndex, Self::Error> {
        Ok(block_index(snapshot.best_tip()))
    }

    async fn find_fork_point(
        &self,
        snapshot: &Self::Snapshot,
        hash: &types::BlockHash,
    ) -> Result<Option<(types::BlockHash, types::Height)>, Self::Error> {
        Ok(snapshot
            .fork_point(&Locator::new(vec![domain_hash(*hash)]))
            .await?
            .map(|fork| (local_hash(fork.hash), local_height(fork.height))))
    }

    async fn get_treestate(
        &self,
        hash: &types::BlockHash,
    ) -> Result<
        (
            Option<PoolTreestate>,
            Option<PoolTreestate>,
            Option<PoolTreestate>,
        ),
        Self::Error,
    > {
        let snapshot = self.composer.snapshot();
        if !Self::block_hash_known(&snapshot, *hash).await? {
            return Err(ChainIndexError::internal(format!(
                "block hash {hash} not found in local chain index"
            )));
        }
        let treestate = snapshot
            .treestate(BlockId::Hash(domain_hash(*hash)))
            .await?
            .ok_or_else(|| {
                ChainIndexError::internal(format!(
                    "failed to fetch treestate for block {hash} from validator"
                ))
            })?;
        Ok((treestate.sapling, treestate.orchard, treestate.ironwood))
    }

    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> Result<Vec<([u8; 32], u32)>, Self::Error> {
        Ok(self
            .composer
            .snapshot()
            .subtree_roots(ShieldedPool::to_domain(pool), start_index, max_entries)
            .await?
            .into_iter()
            .map(|root| (<[u8; 32]>::from(root.root), u32::from(root.end_height)))
            .collect())
    }

    async fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<zaino_primitives::types::AddressBalance, Self::Error> {
        let addresses = balance_request_addresses(&address_strings)?;
        Ok(self.composer.snapshot().address_balance(&addresses).await?)
    }

    async fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<types::TransactionHash>, Self::Error> {
        let (addresses, start, end) = request.into_parts();
        let addresses = encoded_addresses(addresses)?;
        Ok(self
            .composer
            .snapshot()
            .address_txids(&addresses, request_height(start)?, request_height(end)?)
            .await?
            .into_iter()
            .map(local_txid)
            .collect())
    }

    async fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<Vec<zaino_primitives::types::Utxo>, Self::Error> {
        let addresses = balance_request_addresses(&address_strings)?;
        Ok(self.composer.snapshot().address_utxos(&addresses).await?)
    }

    async fn get_outpoint_spenders(
        &self,
        snapshot: &Self::Snapshot,
        outpoints: Vec<types::Outpoint>,
        scope: types::ChainScope,
    ) -> Result<Vec<Option<types::TransactionHash>>, Self::Error> {
        let outpoints: Vec<_> = outpoints
            .iter()
            .map(zaino_chain_store_zainodb::adapter::domain_outpoint)
            .collect();
        snapshot
            .outpoint_spenders(&outpoints, domain_scope(scope))
            .await?
            .into_iter()
            .map(types::spender)
            .collect()
    }
}
