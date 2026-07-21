//! Compile-time proof that `zaino-wallet`'s published client can service
//! zallet's `Chain` / `ChainView` trait.
//!
//! SCAFFOLD: the `mirror` module below is a faithful *shape* copy of
//! zcash/zallet@main `zallet-core/src/components/chain.rs` (default, no
//! `spend-index` feature), with placeholder foreign types so this crate builds
//! without zallet's full dependency tree. The real acid test — implementing
//! zallet's ACTUAL trait — is one line away: flip the commented `zallet-core`
//! git dependency in `Cargo.toml` and delete the mirror.
//!
//! What this file proves *today*: every method of zallet's trait has a home on
//! our client (see the `→` mapping comments). What it cannot prove until the
//! real dep is wired: exact type/lifetime/bound agreement.
#![forbid(unsafe_code)]
#![allow(dead_code)]

use futures::stream::BoxStream;

use zaino_wallet::{WalletClient, WalletSnapshot};
use zaino_service::{IndexerService, Snapshot as InnerSnapshot};

/// Faithful shape-mirror of zallet's backend-neutral chain interface.
mod mirror {
    use futures::stream::BoxStream;
    use std::future::Future;
    use std::ops::Range;

    // --- placeholder foreign types (real: zcash_primitives / zcash_protocol / zebra) ---
    #[derive(Clone, Copy)]
    pub struct Network;
    #[derive(Clone, Copy, PartialEq)]
    pub struct BlockHeight(pub u32);
    #[derive(Clone, Copy, PartialEq)]
    pub struct BlockHash(pub [u8; 32]);
    #[derive(Clone)]
    pub struct Block;
    #[derive(Clone)]
    pub struct BlockHeader;
    #[derive(Clone)]
    pub struct Transaction;
    #[derive(Clone, Copy)]
    pub struct TxId(pub [u8; 32]);
    #[derive(Clone)]
    pub struct TransparentAddress;
    #[derive(Clone)]
    pub struct ChainState;
    #[derive(Clone)]
    pub struct ChainTx;
    #[derive(Clone, Copy)]
    pub struct ChainBlock {
        pub height: BlockHeight,
        pub hash: BlockHash,
    }
    pub struct BlockLocator(pub Vec<BlockHash>);
    #[derive(Debug)]
    pub struct ChainError;
    #[derive(Debug)]
    pub struct Error;
    #[derive(Clone, Copy)]
    pub enum TransactionStatus {
        Mined(BlockHeight),
        Orphaned,
        Unknown,
    }
    #[derive(Clone)]
    pub struct ReportedUpgrade;
    /// Real sig uses `CommitmentTreeRoot<sapling::Node>` etc.; simplified here.
    #[derive(Clone)]
    pub struct SubtreeRoot;

    /// zallet's engine handle. (Mirror.)
    pub trait Chain: Clone + Send + Sync + 'static {
        type View: ChainView;

        fn params(&self) -> &Network;
        fn reported_upgrades(
            &self,
        ) -> impl Future<Output = Result<Vec<ReportedUpgrade>, Error>> + Send;
        fn broadcast_transaction(
            &self,
            tx: &Transaction,
        ) -> impl Future<Output = Result<(), ChainError>> + Send;
        fn get_sapling_subtree_roots(
            &self,
        ) -> impl Future<Output = Result<Vec<SubtreeRoot>, ChainError>> + Send;
        fn get_orchard_subtree_roots(
            &self,
        ) -> impl Future<Output = Result<Vec<SubtreeRoot>, ChainError>> + Send;
        fn get_ironwood_subtree_roots(
            &self,
        ) -> impl Future<Output = Result<Vec<SubtreeRoot>, ChainError>> + Send;
        fn snapshot(&self) -> impl Future<Output = Result<Self::View, ChainError>> + Send;
    }

    /// zallet's pinned view. (Mirror; default no-`spend-index` method set.)
    pub trait ChainView: Clone + Send + Sync + 'static {
        fn tip(&self) -> impl Future<Output = Result<ChainBlock, ChainError>> + Send;
        fn find_fork_point(
            &self,
            locator: &BlockLocator,
        ) -> impl Future<Output = Result<Option<ChainBlock>, ChainError>> + Send;
        fn tree_state_as_of(
            &self,
            height: BlockHeight,
        ) -> impl Future<Output = Result<Option<ChainState>, ChainError>> + Send;
        fn get_block_header(
            &self,
            height: BlockHeight,
        ) -> impl Future<Output = Result<Option<BlockHeader>, ChainError>> + Send;
        fn get_block(
            &self,
            height: BlockHeight,
        ) -> impl Future<Output = Result<Option<Block>, ChainError>> + Send;
        fn stream_blocks_to_tip(
            &self,
            start: BlockHeight,
        ) -> BoxStream<'_, Result<Block, ChainError>>;
        fn stream_blocks(
            &self,
            range: &Range<BlockHeight>,
        ) -> BoxStream<'_, Result<Block, ChainError>>;
        fn get_mempool_stream(
            &self,
        ) -> impl Future<Output = Result<Option<BoxStream<'_, Transaction>>, ChainError>> + Send;
        fn get_transaction(
            &self,
            txid: TxId,
        ) -> impl Future<Output = Result<Option<ChainTx>, ChainError>> + Send;
        fn get_transaction_status(
            &self,
            txid: TxId,
        ) -> impl Future<Output = Result<TransactionStatus, ChainError>> + Send;
        fn get_address_unspent_outpoints(
            &self,
            address: &TransparentAddress,
        ) -> impl Future<Output = Result<Vec<(TxId, u32)>, ChainError>> + Send;
        fn get_address_tx_ids(
            &self,
            address: &TransparentAddress,
            range: Range<BlockHeight>,
        ) -> impl Future<Output = Result<Vec<TxId>, ChainError>> + Send;
    }
}

use mirror::*;

/// zallet's `backends/zaino` adapter, in our tree: wraps our published client
/// and presents it as zallet's `Chain`.
#[derive(Clone)]
pub struct ZainoBackend<E> {
    client: WalletClient<E>,
    network: Network,
}

/// The view side: wraps our published snapshot.
#[derive(Clone)]
pub struct ZainoView<S> {
    snap: WalletSnapshot<S>,
}

impl<E: IndexerService + Clone> Chain for ZainoBackend<E> {
    type View = ZainoView<E::Snapshot>;

    fn params(&self) -> &Network {
        &self.network
    }

    // → WalletClient::reported_upgrades
    async fn reported_upgrades(&self) -> Result<Vec<ReportedUpgrade>, Error> {
        todo!()
    }

    // → WalletClient::broadcast (serialise their &Transaction to bytes, drop the txid)
    async fn broadcast_transaction(&self, _tx: &Transaction) -> Result<(), ChainError> {
        todo!()
    }

    // → WalletClient::{sapling,orchard,ironwood}_subtree_roots
    async fn get_sapling_subtree_roots(&self) -> Result<Vec<SubtreeRoot>, ChainError> {
        todo!()
    }
    async fn get_orchard_subtree_roots(&self) -> Result<Vec<SubtreeRoot>, ChainError> {
        todo!()
    }
    async fn get_ironwood_subtree_roots(&self) -> Result<Vec<SubtreeRoot>, ChainError> {
        todo!()
    }

    // → WalletClient::snapshot
    async fn snapshot(&self) -> Result<Self::View, ChainError> {
        todo!()
    }
}

impl<S: InnerSnapshot + Clone> ChainView for ZainoView<S> {
    // → WalletSnapshot::tip
    async fn tip(&self) -> Result<ChainBlock, ChainError> {
        todo!()
    }
    // → WalletSnapshot::fork_point
    async fn find_fork_point(
        &self,
        _locator: &BlockLocator,
    ) -> Result<Option<ChainBlock>, ChainError> {
        todo!()
    }
    // → WalletSnapshot::treestate
    async fn tree_state_as_of(
        &self,
        _height: BlockHeight,
    ) -> Result<Option<ChainState>, ChainError> {
        todo!()
    }
    // → WalletSnapshot::raw_block_header
    async fn get_block_header(
        &self,
        _height: BlockHeight,
    ) -> Result<Option<BlockHeader>, ChainError> {
        todo!()
    }
    // → WalletSnapshot::raw_block
    async fn get_block(&self, _height: BlockHeight) -> Result<Option<Block>, ChainError> {
        todo!()
    }
    // → WalletSnapshot::stream_blocks_to_tip
    fn stream_blocks_to_tip(&self, _start: BlockHeight) -> BoxStream<'_, Result<Block, ChainError>> {
        todo!()
    }
    // → WalletSnapshot::stream_blocks
    fn stream_blocks(
        &self,
        _range: &std::ops::Range<BlockHeight>,
    ) -> BoxStream<'_, Result<Block, ChainError>> {
        todo!()
    }
    // → WalletSnapshot::mempool_stream
    async fn get_mempool_stream(
        &self,
    ) -> Result<Option<BoxStream<'_, Transaction>>, ChainError> {
        todo!()
    }
    // → WalletSnapshot::raw_transaction
    async fn get_transaction(&self, _txid: TxId) -> Result<Option<ChainTx>, ChainError> {
        todo!()
    }
    // → WalletSnapshot::transaction_status
    async fn get_transaction_status(
        &self,
        _txid: TxId,
    ) -> Result<TransactionStatus, ChainError> {
        todo!()
    }
    // → WalletSnapshot::unspent_outpoints
    async fn get_address_unspent_outpoints(
        &self,
        _address: &TransparentAddress,
    ) -> Result<Vec<(TxId, u32)>, ChainError> {
        todo!()
    }
    // → WalletSnapshot::address_tx_ids
    async fn get_address_tx_ids(
        &self,
        _address: &TransparentAddress,
        _range: std::ops::Range<BlockHeight>,
    ) -> Result<Vec<TxId>, ChainError> {
        todo!()
    }
}
