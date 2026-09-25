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
//! [`OneShotGetTransaction`], [`OneShotGetSubtreeRoots`], the transparent-address
//! reads, and the mempool reads ([`OneShotGetMempoolTxids`],
//! [`OneShotGetRawMempoolTransaction`], [`OneShotGetMempoolCompactTransaction`],
//! [`OneShotGetMempoolSourceTip`])). Each is a mechanical `Deref`-forward.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;

use zaino_primitives::types::{
    AddressBalance, AddressDelta, Block, BlockHash, Height, PreIndexCompactBlock,
    PreIndexCompactTx, ShieldedPool, SubtreeRoot, TransactionId, TreeRoots, Treestate, Utxo,
};

use crate::{
    GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError, GetAddressUtxosError,
    GetBlockByHashError, GetBlockError, GetChainTipError, GetCommitmentTreeRootsError,
    GetMempoolTxidsError, GetRawMempoolTransactionError, GetSubtreeRootsError, GetTransactionError,
    GetTreestateError, OneShotGetAddressBalance, OneShotGetAddressDeltas, OneShotGetAddressTxids,
    OneShotGetAddressUtxos, OneShotGetBlock, OneShotGetBlockByHash, OneShotGetChainTip,
    OneShotGetCommitmentTreeRoots, OneShotGetMempoolCompactTransaction, OneShotGetMempoolSourceTip,
    OneShotGetMempoolTxids, OneShotGetPreIndexCompactBlock, OneShotGetRawMempoolTransaction,
    OneShotGetSubtreeRoots, OneShotGetTransaction, OneShotGetTreestate, OneShotSendRawTransaction,
    QueryError, SendRawTransactionError, SubscribeBlocks, SubscribeChainTip, TipObservation,
    TransactionResponse, ValidatorSource,
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
