//! Darkside RPC implementations.

use zcash_client_backend::proto::service::{
    BlockId, Empty, GetAddressUtxosReply, RawTransaction, TreeState,
};

use crate::{
    define_grpc_passthrough,
    primitives::ProxyClient,
    proto::darkside::{
        darkside_streamer_server::DarksideStreamer, DarksideBlock, DarksideBlocksUrl,
        DarksideEmptyBlocks, DarksideHeight, DarksideMetaState, DarksideSubtreeRoots,
        DarksideTransactionsUrl,
    },
    utils::GrpcConnector,
};

impl DarksideStreamer for ProxyClient {
    /// Reset reverts all darksidewalletd state (active block range, latest height,
    /// staged blocks and transactions) and lightwalletd state (cache) to empty,
    /// the same as the initial state. This occurs synchronously and instantaneously;
    /// no reorg happens in lightwalletd. This is good to do before each independent
    /// test so that no state leaks from one test to another.
    /// Also sets (some of) the values returned by GetLightdInfo(). The Sapling
    /// activation height specified here must be where the block range starts.

    // async fn reset(
    //     &self,
    //     request: tonic::Request<DarksideMetaState>,
    // ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    define_grpc_passthrough!(
        fn reset(
            &self,
            request: tonic::Request<DarksideMetaState>,
        ) -> Empty
    );

    /// StageBlocksStream accepts a list of blocks and saves them into the blocks
    /// staging area until ApplyStaged() is called; there is no immediate effect on
    /// the mock zcashd. Blocks are hex-encoded. Order is important, see ApplyStaged.
    async fn stage_blocks_stream(
        &self,
        request: tonic::Request<tonic::Streaming<DarksideBlock>>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// StageBlocks is the same as StageBlocksStream() except the blocks are fetched
    /// from the given URL. Blocks are one per line, hex-encoded (not JSON).
    async fn stage_blocks(
        &self,
        request: tonic::Request<DarksideBlocksUrl>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// StageBlocksCreate is like the previous two, except it creates 'count'
    /// empty blocks at consecutive heights starting at height 'height'. The
    /// 'nonce' is part of the header, so it contributes to the block hash; this
    /// lets you create identical blocks (same transactions and height), but with
    /// different hashes.
    async fn stage_blocks_create(
        &self,
        request: tonic::Request<DarksideEmptyBlocks>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// StageTransactionsStream stores the given transaction-height pairs in the
    /// staging area until ApplyStaged() is called. Note that these transactions
    /// are not returned by the production GetTransaction() gRPC until they
    /// appear in a "mined" block (contained in the active blockchain presented
    /// by the mock zcashd).
    async fn stage_transactions_stream(
        &self,
        request: tonic::Request<tonic::Streaming<RawTransaction>>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// StageTransactions is the same except the transactions are fetched from
    /// the given url. They are all staged into the block at the given height.
    /// Staging transactions to different heights requires multiple calls.
    async fn stage_transactions(
        &self,
        request: tonic::Request<DarksideTransactionsUrl>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// ApplyStaged iterates the list of blocks that were staged by the
    /// StageBlocks*() gRPCs, in the order they were staged, and "merges" each
    /// into the active, working blocks list that the mock zcashd is presenting
    /// to lightwalletd. Even as each block is applied, the active list can't
    /// have gaps; if the active block range is 1000-1006, and the staged block
    /// range is 1003-1004, the resulting range is 1000-1004, with 1000-1002
    /// unchanged, blocks 1003-1004 from the new range, and 1005-1006 dropped.
    ///
    /// After merging all blocks, ApplyStaged() appends staged transactions (in
    /// the order received) into each one's corresponding (by height) block
    /// The staging area is then cleared.
    ///
    /// The argument specifies the latest block height that mock zcashd reports
    /// (i.e. what's returned by GetLatestBlock). Note that ApplyStaged() can
    /// also be used to simply advance the latest block height presented by mock
    /// zcashd. That is, there doesn't need to be anything in the staging area.
    async fn apply_staged(
        &self,
        request: tonic::Request<DarksideHeight>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// Server streaming response type for the GetIncomingTransactions method.
    type GetIncomingTransactionsStream: tonic::codegen::tokio_stream::Stream<
            Item = std::result::Result<RawTransaction, tonic::Status>,
        > + Send
        + 'static;

    /// Calls to the production gRPC SendTransaction() store the transaction in
    /// a separate area (not the staging area); this method returns all transactions
    /// in this separate area, which is then cleared. The height returned
    /// with each transaction is -1 (invalid) since these transactions haven't
    /// been mined yet. The intention is that the transactions returned here can
    /// then, for example, be given to StageTransactions() to get them "mined"
    /// into a specified block on the next ApplyStaged().
    async fn get_incoming_transactions(
        &self,
        request: tonic::Request<Empty>,
    ) -> std::result::Result<tonic::Response<Self::GetIncomingTransactionsStream>, tonic::Status>;

    /// Clear the incoming transaction pool.
    async fn clear_incoming_transactions(
        &self,
        request: tonic::Request<Empty>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// Add a GetAddressUtxosReply entry to be returned by GetAddressUtxos().
    /// There is no staging or applying for these, very simple.
    async fn add_address_utxo(
        &self,
        request: tonic::Request<GetAddressUtxosReply>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// Clear the list of GetAddressUtxos entries (can't fail)
    async fn clear_address_utxo(
        &self,
        request: tonic::Request<Empty>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// Adds a GetTreeState to the tree state cache
    async fn add_tree_state(
        &self,
        request: tonic::Request<TreeState>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// Removes a GetTreeState for the given height from cache if present (can't fail)
    async fn remove_tree_state(
        &self,
        request: tonic::Request<BlockId>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// Clear the list of GetTreeStates entries (can't fail)
    async fn clear_all_tree_states(
        &self,
        request: tonic::Request<Empty>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

    /// Sets the subtree roots cache (for GetSubtreeRoots),
    /// replacing any existing entries
    async fn set_subtree_roots(
        &self,
        request: tonic::Request<DarksideSubtreeRoots>,
    ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;
}
