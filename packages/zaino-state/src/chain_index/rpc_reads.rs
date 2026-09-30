//! `ChainIndexRpcExt`: compact blocks and the txout set from the chain view,
//! node-only RPCs from the validator.

use futures::StreamExt as _;
use hex::ToHex as _;
use zaino_chain::{CompactBlockRead as _, TxOutSetRead as _};
use zaino_chain_store_zainodb::conversion::{compact_block_to_wire, pool_filter_from_wire};
use zaino_consensus::validate_raw_transaction_hex;
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, MiningInfo,
    NodeInfo, PeerInfo,
};
use zaino_primitives::types::{MempoolInfo, TxOutSetInfo};
use zaino_proto::proto::utils::PoolTypeFilter;
use zebra_rpc::methods::GetBlock;
use zebra_state::HashOrHeight;

use super::chain_head::WithChainHeadSource;
use super::chain_store::WithChainStoreSource;
use super::chain_view::{BestTip as _, WithChainViewSource};
use super::node_backed::NodeBackedChainIndexSubscriber;
use super::reads::flatten;
use super::source::{BlockchainSource, BlockchainSourceResult};
use super::types::{self, domain_height};
use super::{ChainIndex, ChainIndexRpcExt};
use crate::error::ChainIndexError;
use crate::CompactBlockStream;

/// A validator answer, with its failure in this crate's vocabulary.
fn validator<T>(result: BlockchainSourceResult<T>) -> Result<T, ChainIndexError> {
    result.map_err(ChainIndexError::backing_validator)
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > ChainIndexRpcExt for NodeBackedChainIndexSubscriber<Source>
{
    async fn get_compact_block(
        &self,
        snapshot: &Self::Snapshot,
        height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<zaino_proto::proto::compact_formats::CompactBlock>, Self::Error> {
        let Some(at) = domain_height(height).filter(|at| *at <= snapshot.best_tip().height) else {
            return Ok(None);
        };
        let block = snapshot
            .compact_block(at, pool_filter_from_wire(&pool_types))
            .await?
            .ok_or_else(|| ChainIndexError::database_hole(height, None))?;
        Ok(Some(compact_block_to_wire(&block)))
    }

    async fn get_compact_block_stream(
        &self,
        snapshot: &Self::Snapshot,
        start_height: types::Height,
        end_height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<CompactBlockStream>, Self::Error> {
        let tip = u32::from(snapshot.best_tip().height);
        let ascending = start_height <= end_height;
        if !ascending && start_height.0 > tip {
            return Ok(None);
        }
        // Ascending past the tip: serve up to the tip, then say what was cut.
        let past_tip = ascending && end_height.0 > tip;
        let end = if past_tip {
            types::Height(tip)
        } else {
            end_height
        };
        let blocks = match (
            ascending && start_height.0 > tip,
            domain_height(start_height),
            domain_height(end),
        ) {
            (false, Some(start), Some(end)) => Some(flatten(snapshot.stream_compact(
                start,
                end,
                pool_filter_from_wire(&pool_types),
            ))),
            _ => None,
        };

        let (sender, receiver) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move {
            if let Some(blocks) = blocks {
                let mut blocks = std::pin::pin!(blocks);
                while let Some(block) = blocks.next().await {
                    let item = block
                        .map(|block| compact_block_to_wire(&block))
                        .map_err(|error| tonic::Status::internal(error.to_string()));
                    if sender.send(item).await.is_err() {
                        return;
                    }
                }
            }
            if past_tip {
                let _ = sender
                    .send(Err(tonic::Status::out_of_range(format!(
                        "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                        end_height.0, tip,
                    ))))
                    .await;
            }
        });
        Ok(Some(CompactBlockStream::new(receiver)))
    }

    async fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        // Tip-relative negative heights resolve against the best chaintip, as
        // zebra's `getblock` does (`-1` is the tip). A rejected identifier
        // carries the legacy `InvalidParameter` code for the serving layer.
        let tip = self.snapshot_nonfinalized_state().best_tip().height;
        let id = HashOrHeight::new(
            &hash_or_height,
            Some(zebra_chain::block::Height(u32::from(tip))),
        )
        .map_err(|error| {
            ChainIndexError::internal_from(crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::InvalidParameter,
                error,
            ))
        })?;
        validator(self.source().get_block_verbose(id, verbosity).await)
    }

    async fn get_block_header(&self, hash: String) -> Result<BlockHeaderVerbose, Self::Error> {
        validator(self.source().get_block_header(hash).await)
    }

    async fn get_raw_block_header(&self, hash: String) -> Result<Vec<u8>, Self::Error> {
        validator(self.source().get_raw_block_header(hash).await)
    }

    async fn get_block_deltas(&self, hash: String) -> Result<BlockDeltas, Self::Error> {
        validator(self.source().get_block_deltas(hash).await)
    }

    async fn get_difficulty(&self) -> Result<f64, Self::Error> {
        validator(self.source().get_difficulty().await)
    }

    async fn get_info(&self) -> Result<NodeInfo, Self::Error> {
        validator(self.source().get_info().await)
    }

    async fn get_blockchain_info(
        &self,
    ) -> Result<zaino_primitives::types::BlockchainInfo, Self::Error> {
        validator(self.source().get_blockchain_info().await)
    }

    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, Self::Error> {
        validator(self.source().get_peer_info().await)
    }

    async fn get_block_subsidy(&self, height: u32) -> Result<BlockSubsidy, Self::Error> {
        validator(self.source().get_block_subsidy(height).await)
    }

    async fn get_mining_info(&self) -> Result<MiningInfo, Self::Error> {
        validator(self.source().get_mining_info().await)
    }

    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> Result<Option<zaino_primitives::types::rpc::TxOut>, Self::Error> {
        validator(self.source().get_tx_out(txid, n, include_mempool).await)
    }

    async fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> Result<zaino_primitives::types::rpc::SpentInfo, Self::Error> {
        validator(self.source().get_spent_info(outpoint).await)
    }

    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> Result<u64, Self::Error> {
        validator(self.source().get_network_sol_ps(blocks, height).await)
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<zaino_primitives::types::TransactionId, Self::Error> {
        // Rejected locally before the validator round trip, with the legacy
        // `InvalidParameter` code the validator would have given.
        validate_raw_transaction_hex(&raw_transaction_hex).map_err(|error| {
            ChainIndexError::internal_from(crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::InvalidParameter,
                error.to_string(),
            ))
        })?;
        validator(
            self.source()
                .send_raw_transaction(raw_transaction_hex)
                .await,
        )
    }

    async fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> Result<zaino_primitives::types::Treestate, Self::Error> {
        validator(self.source().get_treestate_by_id(hash_or_height).await)
    }

    async fn get_address_deltas(
        &self,
        params: AddressDeltasRequest,
    ) -> Result<AddressDeltas, Self::Error> {
        validator(self.source().get_address_deltas(params).await)
    }

    async fn get_mempool_info(&self) -> MempoolInfo {
        // The tip-agnostic set: this reports what is in the mempool, not where
        // the chain is, so it must not freeze.
        let info = self.mempool.get_mempool_info();
        MempoolInfo {
            size: info.size,
            bytes: info.bytes,
            usage: info.usage,
        }
    }

    async fn get_tx_out_set_info(&self) -> Result<Option<TxOutSetInfo>, Self::Error> {
        let snapshot = self.snapshot_nonfinalized_state();
        let set = match snapshot.txout_set().await {
            Ok(set) => set,
            Err(zaino_chain::ChainViewError::NotServiceable(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let tip = snapshot.best_tip();
        Ok(Some(TxOutSetInfo {
            height: tip.height,
            best_block: tip.hash,
            transactions: set.transactions,
            tx_outs: set.transaction_outputs,
            bytes_serialized: set.bytes_serialized,
            hash_serialized: set.hash_serialized.encode_hex(),
            total_amount: zaino_primitives::types::Zatoshis::new(set.total_zatoshis)
                .map_err(|e| ChainIndexError::internal(e.to_string()))?,
        }))
    }
}
