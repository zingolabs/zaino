//! Regression test for zingolabs/zaino#1081.
//!
//! `TonicServer::spawn` (in the parent module) currently returns
//! `Ok(TonicServer { ... })` before its inner `server_future.await` does
//! the actual TCP bind. If that bind fails (e.g. `EADDRINUSE`), the
//! error is swallowed inside the spawned serve task, and the parent
//! serve loop in `zainod::indexer::Indexer::launch_inner` keeps running
//! indefinitely with the status board claiming everything is fine.
//!
//! The fix per the issue: bind synchronously before constructing the
//! spawn so a bind failure surfaces as `Err` to the caller.
//!
//! This test pre-binds the gRPC port, then calls `TonicServer::spawn`
//! against the occupied address and asserts that the call returns `Err`.
//! Today the call returns `Ok` and this test fails — that's the bug.
//! After the fix lands, the synchronous bind surfaces `AddrInUse` as
//! `Err`, and the test passes.
//!
//! `StubSubscriber` exists only to satisfy the
//! `IndexerSubscriber<S: ZcashIndexer + LightWalletIndexer>` type
//! parameter on `TonicServer::spawn`. Its method bodies are never
//! invoked: the bind fails before any RPC dispatches, both in the
//! current buggy implementation (the spawned serve task dies before
//! accepting a connection) and after the fix (the synchronous bind
//! returns `Err` before the spawn).

use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::TcpListener;
use tonic::async_trait;
use zaino_fetch::jsonrpsee::response::{
    address_deltas::{GetAddressDeltasParams, GetAddressDeltasResponse},
    block_deltas::BlockDeltas,
    block_header::GetBlockHeader,
    block_subsidy::GetBlockSubsidy,
    chain_tips::GetChainTipsResponse,
    mining_info::GetMiningInfoWire,
    peer_info::GetPeerInfo,
    z_validate_address::ZValidateAddressResponse,
    GetMempoolInfoResponse, GetNetworkSolPsResponse, GetSpentInfoRequest, GetSpentInfoResponse,
    GetTxOutResponse,
};
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, Balance, BlockId, BlockRange, Duration, GetAddressUtxosArg,
        GetAddressUtxosReplyList, GetMempoolTxRequest, LightdInfo, PingResponse, RawTransaction,
        SendResponse, TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};
use zaino_state::{
    AddressStream, CompactBlockStream, CompactTransactionStream, IndexerSubscriber,
    LightWalletIndexer, RawTransactionStream, UtxoReplyStream, ZcashIndexer,
};
use zebra_chain::{block::Height, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::{
    client::{GetSubtreesByIndexResponse, GetTreestateResponse, ValidateAddressResponse},
    methods::{
        AddressBalance, GetAddressBalanceRequest, GetAddressTxIdsRequest, GetAddressUtxos,
        GetBlock, GetBlockHash, GetBlockchainInfoResponse, GetInfo, GetRawTransaction,
        SentTransactionHash,
    },
};

use crate::server::{config::GrpcServerConfig, grpc::TonicServer};

#[derive(Clone)]
struct StubSubscriber;

macro_rules! stub {
    () => {
        unimplemented!(
            "StubSubscriber methods must not be invoked: TonicServer::spawn \
             returns before any RPC is dispatched"
        )
    };
}

#[async_trait]
impl ZcashIndexer for StubSubscriber {
    type Error = tonic::Status;

    async fn get_info(&self) -> Result<GetInfo, Self::Error> {
        stub!()
    }
    async fn get_address_deltas(
        &self,
        _: GetAddressDeltasParams,
    ) -> Result<GetAddressDeltasResponse, Self::Error> {
        stub!()
    }
    async fn get_blockchain_info(&self) -> Result<GetBlockchainInfoResponse, Self::Error> {
        stub!()
    }
    async fn get_difficulty(&self) -> Result<f64, Self::Error> {
        stub!()
    }
    async fn get_block_subsidy(&self, _: u32) -> Result<GetBlockSubsidy, Self::Error> {
        stub!()
    }
    async fn get_mempool_info(&self) -> Result<GetMempoolInfoResponse, Self::Error> {
        stub!()
    }
    async fn get_peer_info(&self) -> Result<GetPeerInfo, Self::Error> {
        stub!()
    }
    async fn z_get_address_balance(
        &self,
        _: GetAddressBalanceRequest,
    ) -> Result<AddressBalance, Self::Error> {
        stub!()
    }
    async fn send_raw_transaction(&self, _: String) -> Result<SentTransactionHash, Self::Error> {
        stub!()
    }
    async fn get_block_header(&self, _: String, _: bool) -> Result<GetBlockHeader, Self::Error> {
        stub!()
    }
    async fn z_get_block(&self, _: String, _: Option<u8>) -> Result<GetBlock, Self::Error> {
        stub!()
    }
    async fn get_block_deltas(&self, _: String) -> Result<BlockDeltas, Self::Error> {
        stub!()
    }
    async fn get_block_count(&self) -> Result<Height, Self::Error> {
        stub!()
    }
    async fn get_chain_tips(&self) -> Result<GetChainTipsResponse, Self::Error> {
        stub!()
    }
    async fn validate_address(&self, _: String) -> Result<ValidateAddressResponse, Self::Error> {
        stub!()
    }
    #[allow(deprecated)]
    async fn z_validate_address(&self, _: String) -> Result<ZValidateAddressResponse, Self::Error> {
        stub!()
    }
    async fn get_best_blockhash(&self) -> Result<GetBlockHash, Self::Error> {
        stub!()
    }
    async fn get_raw_mempool(&self) -> Result<Vec<String>, Self::Error> {
        stub!()
    }
    async fn z_get_treestate(&self, _: String) -> Result<GetTreestateResponse, Self::Error> {
        stub!()
    }
    async fn z_get_subtrees_by_index(
        &self,
        _: String,
        _: NoteCommitmentSubtreeIndex,
        _: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<GetSubtreesByIndexResponse, Self::Error> {
        stub!()
    }
    async fn get_raw_transaction(
        &self,
        _: String,
        _: Option<u8>,
    ) -> Result<GetRawTransaction, Self::Error> {
        stub!()
    }
    async fn get_tx_out(
        &self,
        _: String,
        _: u32,
        _: Option<bool>,
    ) -> Result<GetTxOutResponse, Self::Error> {
        stub!()
    }
    async fn get_spent_info(
        &self,
        _: GetSpentInfoRequest,
    ) -> Result<GetSpentInfoResponse, Self::Error> {
        stub!()
    }
    async fn get_address_tx_ids(
        &self,
        _: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, Self::Error> {
        stub!()
    }
    async fn z_get_address_utxos(
        &self,
        _: GetAddressBalanceRequest,
    ) -> Result<Vec<GetAddressUtxos>, Self::Error> {
        stub!()
    }
    async fn get_mining_info(&self) -> Result<GetMiningInfoWire, Self::Error> {
        stub!()
    }
    async fn get_network_sol_ps(
        &self,
        _: Option<i32>,
        _: Option<i32>,
    ) -> Result<GetNetworkSolPsResponse, Self::Error> {
        stub!()
    }
    async fn chain_height(&self) -> Result<Height, Self::Error> {
        stub!()
    }
}

#[async_trait]
impl LightWalletIndexer for StubSubscriber {
    async fn get_latest_block(&self) -> Result<BlockId, Self::Error> {
        stub!()
    }
    async fn get_block(&self, _: BlockId) -> Result<CompactBlock, Self::Error> {
        stub!()
    }
    async fn get_block_nullifiers(&self, _: BlockId) -> Result<CompactBlock, Self::Error> {
        stub!()
    }
    async fn get_block_range(&self, _: BlockRange) -> Result<CompactBlockStream, Self::Error> {
        stub!()
    }
    async fn get_block_range_nullifiers(
        &self,
        _: BlockRange,
    ) -> Result<CompactBlockStream, Self::Error> {
        stub!()
    }
    async fn get_transaction(&self, _: TxFilter) -> Result<RawTransaction, Self::Error> {
        stub!()
    }
    async fn send_transaction(&self, _: RawTransaction) -> Result<SendResponse, Self::Error> {
        stub!()
    }
    async fn get_taddress_transactions(
        &self,
        _: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        stub!()
    }
    async fn get_taddress_txids(
        &self,
        _: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        stub!()
    }
    async fn get_taddress_balance(&self, _: AddressList) -> Result<Balance, Self::Error> {
        stub!()
    }
    async fn get_taddress_balance_stream(&self, _: AddressStream) -> Result<Balance, Self::Error> {
        stub!()
    }
    async fn get_mempool_tx(
        &self,
        _: GetMempoolTxRequest,
    ) -> Result<CompactTransactionStream, Self::Error> {
        stub!()
    }
    async fn get_mempool_stream(&self) -> Result<RawTransactionStream, Self::Error> {
        stub!()
    }
    async fn get_tree_state(&self, _: BlockId) -> Result<TreeState, Self::Error> {
        stub!()
    }
    async fn get_latest_tree_state(&self) -> Result<TreeState, Self::Error> {
        stub!()
    }
    fn timeout_channel_size(&self) -> (u32, u32) {
        stub!()
    }
    async fn get_address_utxos(
        &self,
        _: GetAddressUtxosArg,
    ) -> Result<GetAddressUtxosReplyList, Self::Error> {
        stub!()
    }
    async fn get_address_utxos_stream(
        &self,
        _: GetAddressUtxosArg,
    ) -> Result<UtxoReplyStream, Self::Error> {
        stub!()
    }
    async fn get_lightd_info(&self) -> Result<LightdInfo, Self::Error> {
        stub!()
    }
    async fn ping(&self, _: Duration) -> Result<PingResponse, Self::Error> {
        stub!()
    }
}

#[tokio::test]
async fn returns_err_when_port_is_in_use() {
    // Occupy the port. The held listener stands in for "some other
    // process on the host already holds the gRPC port" from the issue's
    // Production impact section.
    let occupier = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind on ephemeral port must succeed");
    let occupied_addr = occupier
        .local_addr()
        .expect("local_addr on a bound listener is infallible");

    let result = TonicServer::spawn(
        IndexerSubscriber::new(StubSubscriber),
        GrpcServerConfig {
            listen_address: occupied_addr,
            tls: None,
        },
    )
    .await;

    // Today: `result` is `Ok(TonicServer)` because the bind happens
    // inside the spawned serve task, which dies silently with AddrInUse —
    // this assertion FAILS, demonstrating the bug. After #1081 is fixed
    // (synchronous bind), the bind error is propagated as `Err` and this
    // assertion PASSES.
    assert!(
        result.is_err(),
        "TonicServer::spawn must propagate bind failures synchronously \
         (zingolabs/zaino#1081), but returned Ok against an occupied \
         port. The bind error was swallowed inside the spawned serve \
         task."
    );

    // If the buggy path returned Ok, drop the TonicServer here so its
    // Drop impl aborts the doomed serve task before the test exits.
    drop(result);
    drop(occupier);
}
