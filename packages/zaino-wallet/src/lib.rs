//! `zaino-wallet` — the full-wallet library adapter.
//!
//! A stable-DTO facade over the [`WalletLibService`] inner port. A wallet that
//! embeds Zaino (zallet) holds an [`Indexer`] and speaks its DTOs; the inner
//! surface's domain types are converted here, at the adapter, so the inner
//! primitives can evolve without breaking the embedded consumer.
//!
//! The handle is named [`Indexer`] because that is what it is *from the wallet's
//! point of view*: the thing it queries for chain data. It is bound to
//! [`WalletLibService`] alone — not the whole inner surface — so it depends on
//! exactly the capabilities the full-wallet use case needs, and nothing else.
//!
//! This is the driving-adapter half of the hexagon: a public library *is* a
//! driving adapter with its own stable types.
#![forbid(unsafe_code)]

mod dto;
mod error;

pub use dto::{WalletTip, WalletTxId};
pub use error::WalletError;

use zaino_service::{ChainSegment, WalletLibService};

/// The indexer a full wallet library queries, over a [`WalletLibService`] engine.
pub struct Indexer<W: WalletLibService> {
    engine: W,
}

impl<W: WalletLibService> Indexer<W> {
    pub fn new(engine: W) -> Self {
        Self { engine }
    }

    /// The tip the current best chain is pinned to, as a wallet DTO. `None` when
    /// the chain has no tip yet.
    pub async fn tip(&self) -> Result<Option<WalletTip>, WalletError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot.pinned_tip().map(WalletTip::from_domain))
    }

    /// Relay a signed transaction; returns its id.
    pub async fn broadcast(&self, raw_tx: Vec<u8>) -> Result<WalletTxId, WalletError> {
        let id = self.engine.broadcast(raw_tx).await?;
        Ok(WalletTxId::from_domain(id))
    }
}

#[cfg(test)]
mod tests {
    use super::{Indexer, WalletTip, WalletTxId};
    use zaino_core::{BlockHash, BlockId, Height};
    use zaino_service::testing::{MockChain, MockIndexerService};

    /// The adapter binds only `WalletLibService`, pins a snapshot, and maps the
    /// domain tip to a stable DTO.
    #[tokio::test]
    async fn tip_maps_to_dto() {
        let tip = BlockId {
            height: Height::try_from(42).expect("valid height"),
            hash: BlockHash::from([7u8; 32]),
        };
        let indexer = Indexer::new(MockIndexerService::new(MockChain {
            tip: Some(tip),
            ..Default::default()
        }));

        let dto = indexer.tip().await.expect("tip").expect("some tip");
        assert_eq!(
            dto,
            WalletTip {
                height: 42,
                hash: [7u8; 32]
            }
        );
    }

    /// Broadcast routes through the control capability and maps the id out.
    #[tokio::test]
    async fn broadcast_returns_txid_dto() {
        let indexer = Indexer::new(MockIndexerService::new(MockChain::default()));
        let id = indexer.broadcast(vec![1, 2, 3]).await.expect("broadcast");
        assert_eq!(id, WalletTxId([0u8; 32]));
    }
}
