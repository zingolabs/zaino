//! Darkside RPC implementations.

use zcash_client_backend::proto::service::{
    BlockId, Empty, GetAddressUtxosReply, RawTransaction, TreeState,
};

use crate::{
    primitives::ProxyClient,
    proto::darkside::{
        darkside_streamer_server::DarksideStreamer, DarksideBlock, DarksideBlocksUrl,
        DarksideEmptyBlocks, DarksideHeight, DarksideMetaState, DarksideSubtreeRoots,
        DarksideTransactionsUrl,
    },
};

impl DarksideStreamer for ProxyClient {
    /// Reset reverts all darksidewalletd state (active block range, latest height,
    /// staged blocks and transactions) and lightwalletd state (cache) to empty,
    /// the same as the initial state. This occurs synchronously and instantaneously;
    /// no reorg happens in lightwalletd. This is good to do before each independent
    /// test so that no state leaks from one test to another.
    /// Also sets (some of) the values returned by GetLightdInfo(). The Sapling
    /// activation height specified here must be where the block range starts.
    fn reset<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<DarksideMetaState>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// StageBlocksStream accepts a list of blocks and saves them into the blocks
    /// staging area until ApplyStaged() is called; there is no immediate effect on
    /// the mock zcashd. Blocks are hex-encoded. Order is important, see ApplyStaged.
    fn stage_blocks_stream<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<tonic::Streaming<DarksideBlock>>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// StageBlocks is the same as StageBlocksStream() except the blocks are fetched
    /// from the given URL. Blocks are one per line, hex-encoded (not JSON).
    fn stage_blocks<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<DarksideBlocksUrl>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// StageBlocksCreate is like the previous two, except it creates 'count'
    /// empty blocks at consecutive heights starting at height 'height'. The
    /// 'nonce' is part of the header, so it contributes to the block hash; this
    /// lets you create identical blocks (same transactions and height), but with
    /// different hashes.
    fn stage_blocks_create<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<DarksideEmptyBlocks>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// StageTransactionsStream stores the given transaction-height pairs in the
    /// staging area until ApplyStaged() is called. Note that these transactions
    /// are not returned by the production GetTransaction() gRPC until they
    /// appear in a "mined" block (contained in the active blockchain presented
    /// by the mock zcashd).
    fn stage_transactions_stream<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<tonic::Streaming<RawTransaction>>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// StageTransactions is the same except the transactions are fetched from
    /// the given url. They are all staged into the block at the given height.
    /// Staging transactions to different heights requires multiple calls.
    fn stage_transactions<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<DarksideTransactionsUrl>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

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
    fn apply_staged<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<DarksideHeight>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// Server streaming response type for the GetIncomingTransactions method.
    type GetIncomingTransactionsStream = tonic::Streaming<RawTransaction>;

    /// Calls to the production gRPC SendTransaction() store the transaction in
    /// a separate area (not the staging area); this method returns all transactions
    /// in this separate area, which is then cleared. The height returned
    /// with each transaction is -1 (invalid) since these transactions haven't
    /// been mined yet. The intention is that the transactions returned here can
    /// then, for example, be given to StageTransactions() to get them "mined"
    /// into a specified block on the next ApplyStaged().
    fn get_incoming_transactions<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<Empty>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<
                        tonic::Response<Self::GetIncomingTransactionsStream>,
                        tonic::Status,
                    >,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// Clear the incoming transaction pool.
    fn clear_incoming_transactions<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<Empty>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// Add a GetAddressUtxosReply entry to be returned by GetAddressUtxos().
    /// There is no staging or applying for these, very simple.
    fn add_address_utxo<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<GetAddressUtxosReply>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// Clear the list of GetAddressUtxos entries (can't fail)
    fn clear_address_utxo<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<Empty>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// Adds a GetTreeState to the tree state cache
    fn add_tree_state<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<TreeState>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// Removes a GetTreeState for the given height from cache if present (can't fail)
    fn remove_tree_state<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<BlockId>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// Clear the list of GetTreeStates entries (can't fail)
    fn clear_all_tree_states<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<Empty>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }

    /// Sets the subtree roots cache (for GetSubtreeRoots),
    /// replacing any existing entries
    fn set_subtree_roots<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<DarksideSubtreeRoots>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<tonic::Response<Empty>, tonic::Status>,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        unimplemented!()
    }
}
