//! `Arc` forwarding for the ports the compact indexer consumes.
//!
//! One shared `Arc<V>` backs every driven-port consumer in the composed runtime:
//! each wraps the *same* `Arc<V>` in a [`ValidatorClient`](crate::ValidatorClient),
//! whose resilient port impls require the wrapped type — the `Arc` itself — to
//! provide the matching `OneShot*` ports. These mechanical `Deref`-forwards let
//! a bare `Arc<V>` satisfy those bounds, so a single validator handle serves
//! every consumer without cloning the adapter, and no consumer touches a
//! single-attempt port directly.
//!
//! The forwarded set is exactly the ports the shared `Arc<V>` must satisfy for
//! those consumers: [`ValidatorSource`] (the shared supertrait) plus the
//! compact-indexer path ([`OneShotGetPreIndexCompactBlock`],
//! [`OneShotGetChainTip`], [`SubscribeChainTip`]), the chain head's block
//! reads ([`OneShotGetBlock`], [`OneShotGetBlockByHash`],
//! [`OneShotGetCommitmentTreeRoots`]), and the serving path's
//! passthrough ports ([`OneShotGetTreestate`], [`OneShotSendRawTransaction`],
//! [`OneShotGetTransaction`], [`OneShotGetTransactionVerbose`],
//! [`OneShotGetBlockchainInfo`], [`OneShotGetSubtreeRoots`], the transparent-address
//! reads, and the mempool reads ([`OneShotGetMempoolTxids`],
//! [`OneShotGetMempoolMetadata`], [`OneShotGetRawMempoolTransaction`],
//! [`OneShotGetMempoolCompactTransaction`], [`OneShotGetMempoolSourceTip`])). It
//! also carries the node-RPC/explorer serving reads the composite answers over
//! JSON-RPC: the raw and decoded block forms ([`OneShotGetRawBlock`],
//! [`OneShotGetRawBlockByHash`], [`OneShotGetBlockDecoded`],
//! [`OneShotGetBlockDecodedByHash`]) and the node-operator status reads
//! ([`OneShotGetNodeInfo`], [`OneShotGetMiningInfo`], [`OneShotGetPeerInfo`],
//! [`OneShotGetNetworkSolPs`]). Each is a mechanical `Deref`-forward.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;

use zaino_primitives::types::rpc::{
    BlockHeaderVerbose, MiningInfo, NetworkInfo, NodeInfo, PeerInfo, TxOut,
};
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockVerbose, BlockchainInfo, DecodedBlock,
    Difficulty, Height, OutputIndex, PreIndexCompactBlock, PreIndexCompactTx, ShieldedPool,
    SubtreeRoot, TransactionId, TreeRoots, Treestate, Utxo,
};

use crate::{
    DecodedTransaction, GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError,
    GetAddressUtxosError, GetBlockByHashError, GetBlockError, GetBlockHeaderError,
    GetBlockVerboseError, GetBlockchainInfoError, GetChainTipError, GetCommitmentTreeRootsError,
    GetDifficultyError, GetMempoolMetadataError, GetMempoolTxidsError, GetMiningInfoError,
    GetNetworkInfoError, GetNetworkSolPsError, GetNodeInfoError, GetPeerInfoError,
    GetRawMempoolTransactionError, GetSubtreeRootsError, GetTransactionError,
    GetTransactionVerboseError, GetTreestateError, GetTxOutError, MempoolTxMeta,
    OneShotGetAddressBalance, OneShotGetAddressDeltas, OneShotGetAddressTxids,
    OneShotGetAddressUtxos, OneShotGetBlock, OneShotGetBlockByHash, OneShotGetBlockDecoded,
    OneShotGetBlockDecodedByHash, OneShotGetBlockHeader, OneShotGetBlockVerbose,
    OneShotGetBlockVerboseByHash, OneShotGetBlockchainInfo, OneShotGetChainTip,
    OneShotGetCommitmentTreeRoots, OneShotGetDifficulty, OneShotGetMempoolCompactTransaction,
    OneShotGetMempoolMetadata, OneShotGetMempoolSourceTip, OneShotGetMempoolTxids,
    OneShotGetMiningInfo, OneShotGetNetworkInfo, OneShotGetNetworkSolPs, OneShotGetNodeInfo,
    OneShotGetPeerInfo, OneShotGetPreIndexCompactBlock, OneShotGetRawBlock,
    OneShotGetRawBlockByHash, OneShotGetRawMempoolTransaction, OneShotGetSubtreeRoots,
    OneShotGetTransaction, OneShotGetTransactionVerbose, OneShotGetTreestate, OneShotGetTxOut,
    OneShotPing, OneShotSendRawTransaction, QueryError, SendRawTransactionError, SubscribeBlocks,
    SubscribeChainTip, TipObservation, TransactionResponse, ValidatorSource,
};

impl<V: ValidatorSource + ?Sized> ValidatorSource for Arc<V> {
    type NonDomain = V::NonDomain;
}

impl<V: OneShotGetPreIndexCompactBlock + ?Sized> OneShotGetPreIndexCompactBlock for Arc<V> {
    fn get_pre_index_compact_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<PreIndexCompactBlock, QueryError<GetBlockError, Self::NonDomain>>>
           + Send {
        (**self).get_pre_index_compact_block(height)
    }
}

impl<V: OneShotGetChainTip + ?Sized> OneShotGetChainTip for Arc<V> {
    fn get_chain_tip(
        &self,
    ) -> impl Future<
        Output = Result<(BlockHash, Height), QueryError<GetChainTipError, Self::NonDomain>>,
    > + Send {
        (**self).get_chain_tip()
    }
}

impl<V: SubscribeBlocks + ?Sized> SubscribeBlocks for Arc<V> {
    fn subscribe_to_blocks_received(&self) -> Option<watch::Receiver<()>> {
        (**self).subscribe_to_blocks_received()
    }
}

impl<V: SubscribeChainTip + ?Sized> SubscribeChainTip for Arc<V> {
    fn subscribe_to_chain_tip(&self) -> Option<watch::Receiver<TipObservation>> {
        (**self).subscribe_to_chain_tip()
    }
}

// The chain head's questions reach a validator shared behind the same `Arc<V>`:
// it binds the canonical ports, which the client answers over these.
impl<V: OneShotGetBlock + ?Sized> OneShotGetBlock for Arc<V> {
    fn get_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockError, Self::NonDomain>>> + Send
    {
        (**self).get_block(height)
    }
}

impl<V: OneShotGetBlockByHash + ?Sized> OneShotGetBlockByHash for Arc<V> {
    fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockByHashError, Self::NonDomain>>> + Send
    {
        (**self).get_block_by_hash(hash)
    }
}

impl<V: OneShotGetCommitmentTreeRoots + ?Sized> OneShotGetCommitmentTreeRoots for Arc<V> {
    fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> impl Future<
        Output = Result<TreeRoots, QueryError<GetCommitmentTreeRootsError, Self::NonDomain>>,
    > + Send {
        (**self).get_commitment_tree_roots(block)
    }
}

// The serving path's passthrough reads reach the validator through the same
// `Arc<V>`: treestate today, more as the read-set is wired.
impl<V: OneShotGetTreestate + ?Sized> OneShotGetTreestate for Arc<V> {
    fn get_treestate(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Treestate, QueryError<GetTreestateError, Self::NonDomain>>> + Send
    {
        (**self).get_treestate(height)
    }
}

// The serving path's broadcast control reaches the validator's send port through
// the same `Arc<V>`, so the composed engine can relay a wallet's transaction
// without holding the concrete adapter.
impl<V: OneShotSendRawTransaction + ?Sized> OneShotSendRawTransaction for Arc<V> {
    fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> impl Future<
        Output = Result<TransactionId, QueryError<SendRawTransactionError, Self::NonDomain>>,
    > + Send {
        (**self).send_raw_transaction(transaction)
    }
}

// The serving path's transparent-address reads reach the validator through the
// same `Arc<V>` (a passthrough stopgap pending a local transparent index).
impl<V: OneShotGetAddressBalance + ?Sized> OneShotGetAddressBalance for Arc<V> {
    fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> impl Future<
        Output = Result<AddressBalance, QueryError<GetAddressBalanceError, Self::NonDomain>>,
    > + Send {
        (**self).get_address_balance(addresses)
    }
}

impl<V: OneShotGetAddressUtxos + ?Sized> OneShotGetAddressUtxos for Arc<V> {
    fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> impl Future<Output = Result<Vec<Utxo>, QueryError<GetAddressUtxosError, Self::NonDomain>>> + Send
    {
        (**self).get_address_utxos(addresses)
    }
}

impl<V: OneShotGetAddressTxids + ?Sized> OneShotGetAddressTxids for Arc<V> {
    fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> impl Future<
        Output = Result<Vec<TransactionId>, QueryError<GetAddressTxidsError, Self::NonDomain>>,
    > + Send {
        (**self).get_address_txids(addresses, start, end)
    }
}

impl<V: OneShotGetAddressDeltas + ?Sized> OneShotGetAddressDeltas for Arc<V> {
    fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> impl Future<
        Output = Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError, Self::NonDomain>>,
    > + Send {
        (**self).get_address_deltas(addresses, start, end)
    }
}

// The serving path's raw-transaction fetch and subtree-root paging reach the
// validator through the same `Arc<V>` (both pure passthrough).
impl<V: OneShotGetTransaction + ?Sized> OneShotGetTransaction for Arc<V> {
    fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<
        Output = Result<TransactionResponse, QueryError<GetTransactionError, Self::NonDomain>>,
    > + Send {
        (**self).get_transaction(txid)
    }
}

impl<V: OneShotGetTransactionVerbose + ?Sized> OneShotGetTransactionVerbose for Arc<V> {
    fn get_transaction_verbose(
        &self,
        txid: TransactionId,
    ) -> impl Future<
        Output = Result<
            DecodedTransaction,
            QueryError<GetTransactionVerboseError, Self::NonDomain>,
        >,
    > + Send {
        (**self).get_transaction_verbose(txid)
    }
}

// The explorer's decoded reads (`getrawtransaction`'s verbose form above, and
// the whole-block decode here) reach the validator through the same `Arc<V>`.
impl<V: OneShotGetBlockDecoded + ?Sized> OneShotGetBlockDecoded for Arc<V> {
    fn get_block_decoded(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<DecodedBlock, QueryError<GetBlockError, Self::NonDomain>>> + Send
    {
        (**self).get_block_decoded(height)
    }
}

impl<V: OneShotGetBlockDecodedByHash + ?Sized> OneShotGetBlockDecodedByHash for Arc<V> {
    fn get_block_decoded_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<DecodedBlock, QueryError<GetBlockByHashError, Self::NonDomain>>>
           + Send {
        (**self).get_block_decoded_by_hash(hash)
    }
}

// The serving path's verbose block reads (the explorer's `getblockheader` and
// `getblock`) reach the validator through the same `Arc<V>`: header-plus-chain-
// state, and verbose block metadata addressed by height or by hash.
impl<V: OneShotGetBlockHeader + ?Sized> OneShotGetBlockHeader for Arc<V> {
    fn get_block_header(
        &self,
        hash: BlockHash,
    ) -> impl Future<
        Output = Result<BlockHeaderVerbose, QueryError<GetBlockHeaderError, Self::NonDomain>>,
    > + Send {
        (**self).get_block_header(hash)
    }
}

impl<V: OneShotGetBlockVerbose + ?Sized> OneShotGetBlockVerbose for Arc<V> {
    fn get_block_verbose(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<BlockVerbose, QueryError<GetBlockVerboseError, Self::NonDomain>>>
           + Send {
        (**self).get_block_verbose(height)
    }
}

impl<V: OneShotGetBlockVerboseByHash + ?Sized> OneShotGetBlockVerboseByHash for Arc<V> {
    fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<BlockVerbose, QueryError<GetBlockVerboseError, Self::NonDomain>>>
           + Send {
        (**self).get_block_verbose_by_hash(hash)
    }
}

// The serving path's raw consensus-byte block reads (the explorer's `getblock`
// at verbosity 0) reach the validator through the same `Arc<V>`, by height or by
// hash.
impl<V: OneShotGetRawBlock + ?Sized> OneShotGetRawBlock for Arc<V> {
    fn get_raw_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetBlockError, Self::NonDomain>>> + Send
    {
        (**self).get_raw_block(height)
    }
}

impl<V: OneShotGetRawBlockByHash + ?Sized> OneShotGetRawBlockByHash for Arc<V> {
    fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetBlockByHashError, Self::NonDomain>>> + Send
    {
        (**self).get_raw_block_by_hash(hash)
    }
}

impl<V: OneShotGetBlockchainInfo + ?Sized> OneShotGetBlockchainInfo for Arc<V> {
    fn get_blockchain_info(
        &self,
    ) -> impl Future<
        Output = Result<BlockchainInfo, QueryError<GetBlockchainInfoError, Self::NonDomain>>,
    > + Send {
        (**self).get_blockchain_info()
    }
}

impl<V: OneShotGetSubtreeRoots + ?Sized> OneShotGetSubtreeRoots for Arc<V> {
    fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> impl Future<
        Output = Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError, Self::NonDomain>>,
    > + Send {
        (**self).get_subtree_roots(pool, start_index, limit)
    }
}

// The serving path's mempool passthrough reaches the validator through the same
// `Arc<V>`: the listing, each transaction's bytes, and the coherence tip, all
// from the one source the ports require.
impl<V: OneShotGetMempoolTxids + ?Sized> OneShotGetMempoolTxids for Arc<V> {
    fn get_mempool_txids(
        &self,
    ) -> impl Future<
        Output = Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError, Self::NonDomain>>,
    > + Send {
        (**self).get_mempool_txids()
    }
}

// The verbose mempool listing (`getrawmempool true` / `getmempoolinfo`) reaches
// the validator through the same `Arc<V>`.
impl<V: OneShotGetMempoolMetadata + ?Sized> OneShotGetMempoolMetadata for Arc<V> {
    fn get_mempool_metadata(
        &self,
    ) -> impl Future<
        Output = Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError, Self::NonDomain>>,
    > + Send {
        (**self).get_mempool_metadata()
    }
}

impl<V: OneShotGetRawMempoolTransaction + ?Sized> OneShotGetRawMempoolTransaction for Arc<V> {
    fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<
        Output = Result<Vec<u8>, QueryError<GetRawMempoolTransactionError, Self::NonDomain>>,
    > + Send {
        (**self).get_raw_mempool_transaction(txid)
    }
}

impl<V: OneShotGetMempoolSourceTip + ?Sized> OneShotGetMempoolSourceTip for Arc<V> {
    fn get_mempool_source_tip(
        &self,
    ) -> impl Future<Output = Result<(BlockHash, Height), QueryError<Infallible, Self::NonDomain>>> + Send
    {
        (**self).get_mempool_source_tip()
    }
}

impl<V: OneShotGetMempoolCompactTransaction + ?Sized> OneShotGetMempoolCompactTransaction
    for Arc<V>
{
    fn get_mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<
        Output = Result<
            PreIndexCompactTx,
            QueryError<GetRawMempoolTransactionError, Self::NonDomain>,
        >,
    > + Send {
        (**self).get_mempool_compact_transaction(txid)
    }
}

// The node-operator status reads (the explorer's `getinfo` / `getmininginfo` /
// `getpeerinfo` / `getnetworksolps`) reach the validator through the same
// `Arc<V>`: facts about the validator, not the chain, so always passthrough.
impl<V: OneShotGetNodeInfo + ?Sized> OneShotGetNodeInfo for Arc<V> {
    fn get_node_info(
        &self,
    ) -> impl Future<Output = Result<NodeInfo, QueryError<GetNodeInfoError, Self::NonDomain>>> + Send
    {
        (**self).get_node_info()
    }
}

impl<V: crate::OneShotGetBlockSubsidy + ?Sized> crate::OneShotGetBlockSubsidy for Arc<V> {
    fn get_block_subsidy(
        &self,
        height: Height,
    ) -> impl Future<
        Output = Result<
            zaino_primitives::types::rpc::BlockSubsidy,
            QueryError<crate::GetBlockSubsidyError, Self::NonDomain>,
        >,
    > + Send {
        (**self).get_block_subsidy(height)
    }
}

impl<V: OneShotGetMiningInfo + ?Sized> OneShotGetMiningInfo for Arc<V> {
    fn get_mining_info(
        &self,
    ) -> impl Future<Output = Result<MiningInfo, QueryError<GetMiningInfoError, Self::NonDomain>>> + Send
    {
        (**self).get_mining_info()
    }
}

impl<V: OneShotGetPeerInfo + ?Sized> OneShotGetPeerInfo for Arc<V> {
    fn get_peer_info(
        &self,
    ) -> impl Future<Output = Result<Vec<PeerInfo>, QueryError<GetPeerInfoError, Self::NonDomain>>> + Send
    {
        (**self).get_peer_info()
    }
}

impl<V: OneShotGetNetworkSolPs + ?Sized> OneShotGetNetworkSolPs for Arc<V> {
    fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> impl Future<Output = Result<u64, QueryError<GetNetworkSolPsError, Self::NonDomain>>> + Send
    {
        (**self).get_network_sol_ps(blocks, height)
    }
}

// The chain-wide difficulty and the node's `getnetworkinfo` / `ping` reach the
// validator through the same `Arc<V>`: the explorer's `getdifficulty`,
// `getnetworkinfo` and `ping`.
impl<V: OneShotGetDifficulty + ?Sized> OneShotGetDifficulty for Arc<V> {
    fn get_difficulty(
        &self,
    ) -> impl Future<Output = Result<Difficulty, QueryError<GetDifficultyError, Self::NonDomain>>> + Send
    {
        (**self).get_difficulty()
    }
}

impl<V: OneShotGetNetworkInfo + ?Sized> OneShotGetNetworkInfo for Arc<V> {
    fn get_network_info(
        &self,
    ) -> impl Future<Output = Result<NetworkInfo, QueryError<GetNetworkInfoError, Self::NonDomain>>> + Send
    {
        (**self).get_network_info()
    }
}

impl<V: OneShotPing + ?Sized> OneShotPing for Arc<V> {
    fn ping(
        &self,
    ) -> impl Future<Output = Result<(), QueryError<core::convert::Infallible, Self::NonDomain>>> + Send
    {
        (**self).ping()
    }
}

// The explorer's `gettxout` reaches the validator through the same `Arc<V>`: a
// live unspent-output lookup, always passthrough (not an indexed read).
impl<V: OneShotGetTxOut + ?Sized> OneShotGetTxOut for Arc<V> {
    fn get_tx_out(
        &self,
        txid: TransactionId,
        index: OutputIndex,
        include_mempool: bool,
    ) -> impl Future<Output = Result<Option<TxOut>, QueryError<GetTxOutError, Self::NonDomain>>> + Send
    {
        (**self).get_tx_out(txid, index, include_mempool)
    }
}
