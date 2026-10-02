//! `zaino-noderpc` — POC Zcash node JSON-RPC serve adapter.
//!
//! The node-RPC sibling of the light-serve adapter, bound to [`NodeRpcService`]
//! alone. It reads domain types through a pinned snapshot and converts
//! **domain <-> wire in the adapter** (see [`wire`]) — both directions, because
//! node RPC is input-heavy (hex params in, hex/JSON out).
//!
//! A slice (four methods), not the production JSON-RPC server, and it stands up
//! no jsonrpsee server — it exercises the handler shape against the mock.
//! Chain/node-info aggregates and the validator passthrough (mining/peers/
//! txoutset) are not modelled here.
#![forbid(unsafe_code)]

mod error;
mod rpc;
mod transport;
pub mod wire;

pub use error::RpcError;
pub use rpc::NodeRpcApiServer;
pub use transport::{JsonRpcServeError, JsonRpcServer};

use zaino_primitives::types::{Height, HeightRange, Outpoint, TransparentAddress};
use zaino_service::error::ReadError;
use zaino_service::NodeQuery;
use zaino_service::{AddressRead, ChainInfoRead, ChainSegment, NodeRpcService, SpendRead};

use crate::wire::params::AddressesParam;
use crate::wire::response::AddressBalanceResponse;
use crate::wire::{bytes_from_hex, spend_status_to_wire, to_hex, txid_from_hex};

/// Zcash node JSON-RPC handler over a [`NodeRpcService`] engine.
#[derive(Clone)]
pub struct NodeRpc<S: NodeRpcService> {
    engine: S,
}

impl<S: NodeRpcService> NodeRpc<S> {
    pub fn new(engine: S) -> Self {
        Self { engine }
    }

    /// `getblockcount`: the height of the pinned tip.
    pub async fn get_block_count(&self) -> Result<u32, RpcError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(RpcError::NoBlocks)?;
        Ok(tip.height.into())
    }

    /// `getbestblockhash`: the hash of the pinned tip, as hex.
    pub async fn get_best_block_hash(&self) -> Result<String, RpcError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(RpcError::NoBlocks)?;
        Ok(to_hex(tip.hash.into()))
    }

    /// `gettxout`: a node-RPC-specific read (`SpendRead`) the light path lacks.
    /// Demonstrates fallible wire-input validation (the txid) into a domain read.
    pub async fn get_tx_out(&self, txid_hex: &str, index: u32) -> Result<String, RpcError> {
        let outpoint = Outpoint {
            txid: txid_from_hex(txid_hex)?,
            index,
        };
        let snapshot = self.engine.snapshot().await?;
        let status = snapshot.spend_status(outpoint).await?;
        Ok(spend_status_to_wire(status))
    }

    /// `sendrawtransaction`: decode hex, relay, return the txid. A rejection is
    /// an RPC error here (contrast the light-serve `SendResponse`).
    pub async fn send_raw_transaction(&self, tx_hex: &str) -> Result<String, RpcError> {
        let raw = bytes_from_hex(tx_hex)?;
        let txid = self.engine.broadcast(raw).await?;
        Ok(to_hex(txid.into()))
    }

    /// `getblockchaininfo` (aggregate): reads the domain `ChainInfo` — the
    /// node-rpc read delta the wallet-shaped ports lack.
    pub async fn get_blockchain_info(&self) -> Result<String, RpcError> {
        let snapshot = self.engine.snapshot().await?;
        let info = snapshot.chain_info().await?;
        Ok(format!(
            "estimated_height={}",
            u32::from(info.estimated_height)
        ))
    }

    /// `getmininginfo`: not indexed — relayed to the validator through the
    /// passthrough seam and returned opaque.
    pub async fn get_mining_info(&self) -> Result<String, RpcError> {
        let answer = self.engine.relay_node_query(NodeQuery::MiningInfo).await?;
        Ok(answer.0)
    }

    /// `getaddressbalance`: the transparent balance of the requested addresses,
    /// summed. zcashd accepts a list and returns one total, so a multi-address
    /// request sums rather than returning a per-address breakdown.
    pub async fn get_address_balance(
        &self,
        params: AddressesParam,
    ) -> Result<AddressBalanceResponse, RpcError> {
        if params.addresses.is_empty() {
            return Err(RpcError::InvalidParams(
                "addresses must not be empty".into(),
            ));
        }
        let snapshot = self.engine.snapshot().await?;
        let range = full_range(&snapshot);
        let mut balance: u64 = 0;
        let mut received: u128 = 0;
        for address in params.addresses {
            let read = snapshot
                .balance(&TransparentAddress::new(address), range)
                .await?;
            balance = balance.checked_add(read.balance.as_u64()).ok_or_else(|| {
                RpcError::Read(ReadError::Fatal("summed balance overflows u64".into()))
            })?;
            received = received
                .checked_add(u128::from(read.received))
                .ok_or_else(|| {
                    RpcError::Read(ReadError::Fatal("summed receipts overflow u128".into()))
                })?;
        }
        Ok(AddressBalanceResponse { balance, received })
    }
}

/// The whole serviceable height range of `snapshot`, for the address RPCs,
/// which take no range of their own.
fn full_range(snapshot: &impl ChainSegment) -> HeightRange {
    snapshot.coverage().unwrap_or(HeightRange {
        start: Height::GENESIS,
        end: Height::GENESIS,
    })
}

#[cfg(test)]
mod tests {
    use super::{NodeRpc, RpcError};
    use zaino_primitives::types::{BlockHash, BlockRef, Height};
    use zaino_service::testing::{MockChain, MockIndexerService};

    fn engine_with_tip(tip: Option<BlockRef>) -> MockIndexerService {
        MockIndexerService::new(MockChain {
            tip,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn block_count_and_best_hash_read_the_pinned_tip() {
        let tip = BlockRef {
            height: Height::try_from(291).expect("valid height"),
            hash: BlockHash::from([0xCDu8; 32]),
        };
        let node = NodeRpc::new(engine_with_tip(Some(tip)));
        assert_eq!(node.get_block_count().await.expect("count"), 291);
        assert_eq!(
            node.get_best_block_hash().await.expect("hash"),
            "cd".repeat(32)
        );
    }

    #[tokio::test]
    async fn get_tx_out_validates_the_txid_then_reads() {
        let node = NodeRpc::new(engine_with_tip(None));
        // Well-formed txid -> the (mock) read runs and reports no such output.
        let ok = node.get_tx_out(&"ab".repeat(32), 0).await.expect("read");
        assert_eq!(ok, "none");
        // Malformed txid -> validated at the boundary, never reaches the read.
        assert!(matches!(
            node.get_tx_out("xyz", 0).await,
            Err(RpcError::InvalidParams(_))
        ));
    }

    #[tokio::test]
    async fn send_raw_transaction_decodes_and_relays() {
        let node = NodeRpc::new(engine_with_tip(None));
        let txid = node
            .send_raw_transaction("deadbeef")
            .await
            .expect("broadcast");
        assert_eq!(txid, "0".repeat(64)); // mock returns the zero txid
        assert!(matches!(
            node.send_raw_transaction("odd").await,
            Err(RpcError::InvalidParams(_))
        ));
    }

    #[tokio::test]
    async fn chain_info_reads_and_mining_info_passes_through() {
        let tip = BlockRef {
            height: Height::try_from(77).expect("valid height"),
            hash: BlockHash::from([0u8; 32]),
        };
        let node = NodeRpc::new(engine_with_tip(Some(tip)));
        // Chain-info aggregate: a node-rpc-specific indexed read.
        assert_eq!(
            node.get_blockchain_info().await.expect("chain info"),
            "estimated_height=77"
        );
        // Mining info: not indexed — relayed opaque through the passthrough seam.
        assert!(node
            .get_mining_info()
            .await
            .expect("mining info")
            .contains("MiningInfo"));
    }

    #[tokio::test]
    async fn address_balance_renders_the_scripted_balance() {
        use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum};
        let engine = MockIndexerService::new(MockChain {
            balances: vec![(
                "t1abc".to_string(),
                zaino_primitives::types::AddressBalance {
                    balance: Zatoshis::new(5_000).expect("valid amount"),
                    received: ZatoshisFlowSum::from_summed(12_000),
                },
            )],
            ..Default::default()
        });
        let node = NodeRpc::new(engine);
        let got = node
            .get_address_balance(crate::wire::params::AddressesParam {
                addresses: vec!["t1abc".to_string()],
            })
            .await
            .expect("balance");
        assert_eq!(got.balance, 5_000);
        assert_eq!(got.received, 12_000);
    }

    /// Review Focus 2: an address with no history is zero, not an error.
    #[tokio::test]
    async fn an_address_with_no_history_is_zero_not_an_error() {
        let node = NodeRpc::new(engine_with_tip(None));
        let got = node
            .get_address_balance(crate::wire::params::AddressesParam {
                addresses: vec!["t1nohistory".to_string()],
            })
            .await
            .expect("an unknown address is a valid query");
        assert_eq!(got.balance, 0);
        assert_eq!(got.received, 0);
    }

    #[tokio::test]
    async fn address_balance_rejects_an_empty_address_list() {
        let node = NodeRpc::new(engine_with_tip(None));
        assert!(matches!(
            node.get_address_balance(crate::wire::params::AddressesParam {
                addresses: Vec::new()
            })
            .await,
            Err(RpcError::InvalidParams(_))
        ));
    }
}
