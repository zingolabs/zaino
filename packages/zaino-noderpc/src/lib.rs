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
mod wire;

pub use error::RpcError;

use zaino_core::Outpoint;
use zaino_service::{NodeRpcService, Snapshot, SpendRead};

use crate::wire::{bytes_from_hex, spend_status_to_wire, to_hex, txid_from_hex};

/// Zcash node JSON-RPC handler over a [`NodeRpcService`] engine.
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
}

#[cfg(test)]
mod tests {
    use super::{NodeRpc, RpcError};
    use zaino_core::{BlockHash, BlockId, Height};
    use zaino_service::testing::{MockChain, MockIndexerService};

    fn engine_with_tip(tip: Option<BlockId>) -> MockIndexerService {
        MockIndexerService::new(MockChain {
            tip,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn block_count_and_best_hash_read_the_pinned_tip() {
        let tip = BlockId {
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
}
