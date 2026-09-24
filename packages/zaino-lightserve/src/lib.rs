//! `zaino-lightserve` — POC lightwalletd-compatible gRPC serve adapter.
//!
//! Proof that the new trait algebra rebinds the serving layer cleanly. A handler
//! bound to [`LightServeService`] alone (not the god-traits) pins a snapshot,
//! reads domain types, converts **domain -> wire in the adapter** (see
//! [`wire`]), and maps errors by kind — a transient snapshot failure, a
//! not-yet-serviceable chain, and a domain broadcast rejection are three
//! different wire outcomes, not one fused transport error.
//!
//! The handler covers the compact-block serving path (`GetLatestBlock`,
//! `GetBlock`, `GetBlockRange`, `GetLightdInfo`, `SendTransaction`);
//! [`GrpcServer`] stands up a real tonic `CompactTxStreamer` server over it
//! ([`RunLoop`](zaino_component::RunLoop)), serving those and returning
//! `Status::unimplemented` for the rest of the generated (fixed lightwalletd)
//! contract until their handler methods exist.
#![forbid(unsafe_code)]

mod error;
mod grpc;
mod transport;
mod wire;

pub use error::ServeError;
pub use grpc::GrpcService;
pub use transport::{GrpcServeError, GrpcServer};

use futures::stream::{BoxStream, StreamExt};
use zaino_core::{BlockRef, Height, HeightRange, ShieldedPool, TransactionId, TransparentAddress};
use zaino_proto::proto::compact_formats as compact;
use zaino_proto::proto::service as proto;
use zaino_service::{
    AddressRead, ChainSegment, CompactBlockRead, CompactNullifierRead, LightServeService,
    RawTransactionRead, TreestateRead,
};

use crate::wire::{to_hex, zat_to_i64, ToWire};

/// Lightwalletd-compatible handler over a [`LightServeService`] engine.
#[derive(Clone)]
pub struct LightServe<S: LightServeService> {
    engine: S,
}

impl<S: LightServeService> LightServe<S> {
    pub fn new(engine: S) -> Self {
        Self { engine }
    }

    /// `GetLatestBlock`: the tip of the pinned best chain, as a wire `BlockId`.
    pub async fn get_latest_block(&self) -> Result<proto::BlockId, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(ServeError::NoBlocks)?;
        Ok(tip.to_wire())
    }

    /// `GetLightdInfo`: serving metadata + the current tip height.
    ///
    /// Minimal but valid: `version`/`vendor`/`taddr_support` are static, and
    /// `block_height`/`estimated_height` are read from the pinned tip (0 before
    /// any block is served). The network-derived fields (`chain_name`,
    /// `sapling_activation_height`, `consensus_branch_id`) are left best-effort
    /// empty here — the handler is not parameterised by the network, and the
    /// clients that gate readiness on this call read only the height. Threading
    /// the network through to fill them is a later refinement.
    pub async fn get_lightd_info(&self) -> Result<proto::LightdInfo, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let block_height = snapshot
            .pinned_tip()
            .map(|tip| u64::from(tip.height))
            .unwrap_or(0);
        Ok(proto::LightdInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            vendor: "zaino".to_string(),
            taddr_support: true,
            block_height,
            estimated_height: block_height,
            ..Default::default()
        })
    }

    /// `GetBlock`: the composed compact block at `at`, or `None` when no block
    /// is indexed there (the caller maps that to a not-found status). The
    /// snapshot pins the view; the compact block is composed on read and
    /// converted domain -> wire in the adapter.
    pub async fn get_block(
        &self,
        at: BlockRef,
    ) -> Result<Option<compact::CompactBlock>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot.compact_block(at).await?.map(ToWire::to_wire))
    }

    /// `GetBlockRange`: the composed compact blocks over `range`, as a stream.
    ///
    /// The store composes the range eagerly, so the items are collected owned
    /// and returned as a `'static` stream; a per-block read failure rides the
    /// stream, classified by kind, rather than aborting the pin. Acquiring the
    /// snapshot itself can still fail up front (transient).
    pub async fn get_block_range(
        &self,
        range: HeightRange,
    ) -> Result<BoxStream<'static, Result<compact::CompactBlock, ServeError>>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let blocks: Vec<Result<compact::CompactBlock, ServeError>> = snapshot
            .stream_compact(range)
            .map(|read| read.map(ToWire::to_wire).map_err(ServeError::from))
            .collect()
            .await;
        Ok(Box::pin(futures::stream::iter(blocks)))
    }

    /// `GetTreeState`: the commitment tree state at `height`. Zaino does not
    /// index treestate — the engine's remote view passes it through to the
    /// validator — and the domain answer is converted domain -> wire here.
    pub async fn get_tree_state(&self, height: Height) -> Result<proto::TreeState, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot.treestate(height).await?.to_wire())
    }

    /// `GetLatestTreeState`: the tree state at the pinned tip. `NoBlocks` before
    /// any block is served (there is no tip to key the treestate on).
    pub async fn get_latest_tree_state(&self) -> Result<proto::TreeState, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(ServeError::NoBlocks)?;
        Ok(snapshot.treestate(tip.height).await?.to_wire())
    }

    /// `GetSubtreeRoots`: note-commitment subtree roots for `pool`, a run of at
    /// most `limit` (all when `None`) starting at `start_index`. Passed through to
    /// the validator; collected owned so the gRPC layer serves them as a `'static`
    /// stream.
    pub async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<proto::SubtreeRoot>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let roots = snapshot.subtree_roots(pool, start_index, limit).await?;
        Ok(roots.into_iter().map(ToWire::to_wire).collect())
    }

    /// `GetTransaction`: a transaction as raw bytes, or `None` when the
    /// validator does not know it (the caller maps that to not-found). Passed
    /// through to the validator's raw-transaction fetch.
    pub async fn get_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<proto::RawTransaction>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot.raw_transaction(id).await?.map(ToWire::to_wire))
    }

    /// `GetTaddressBalance`: the total current balance across `addrs`, summed
    /// over the whole indexed chain (genesis .. tip). `NoBlocks` before any block
    /// is served. Each address balance is passed through to the validator.
    pub async fn get_taddress_balance(
        &self,
        addrs: Vec<TransparentAddress>,
    ) -> Result<proto::Balance, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(ServeError::NoBlocks)?;
        let range = HeightRange {
            start: Height::GENESIS,
            end: tip.height,
        };
        let mut total: u64 = 0;
        for addr in &addrs {
            let balance = snapshot.balance(addr, range).await?;
            // Saturating: a sum of supply-bounded balances stays below the money
            // supply, so this never actually saturates.
            total = total.saturating_add(u64::from(balance.balance));
        }
        Ok(proto::Balance {
            value_zat: zat_to_i64(total),
        })
    }

    /// `GetAddressUtxos`: the unspent outputs at or above `start_height` across
    /// `addrs`, capped at `max_entries` (`0` meaning unlimited). Passed through to
    /// the validator; collected owned so the gRPC layer can serve the list or a
    /// `'static` stream.
    pub async fn get_address_utxos(
        &self,
        addrs: Vec<TransparentAddress>,
        start_height: Height,
        max_entries: usize,
    ) -> Result<Vec<proto::GetAddressUtxosReply>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let mut utxos = Vec::new();
        for addr in &addrs {
            for utxo in snapshot.unspent_outpoints(addr).await? {
                if utxo.height >= start_height {
                    utxos.push(utxo.to_wire());
                }
            }
        }
        if max_entries != 0 {
            utxos.truncate(max_entries);
        }
        Ok(utxos)
    }

    /// `GetTaddressTxids`: the transactions touching `addr` within `range`, as
    /// raw bytes. The txids come from the address index; each transaction's bytes
    /// are passed through to the validator (a txid the validator has since dropped
    /// is skipped). Collected owned for a `'static` stream.
    pub async fn get_taddress_txids(
        &self,
        addr: TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<proto::RawTransaction>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let ids = snapshot.tx_ids(&addr, range).await?;
        let mut txs = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(raw) = snapshot.raw_transaction(id).await? {
                txs.push(raw.to_wire());
            }
        }
        Ok(txs)
    }

    /// `GetBlockNullifiers`: the compact block at `at` with spend nullifiers
    /// populated, or `None` when no block is indexed there.
    pub async fn get_block_nullifiers(
        &self,
        at: BlockRef,
    ) -> Result<Option<compact::CompactBlock>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot
            .compact_block_nullifiers(at)
            .await?
            .map(ToWire::to_wire))
    }

    /// `GetBlockRangeNullifiers`: the nullifier-populated compact blocks over
    /// `range`. Composed per height and collected owned for a `'static` stream; a
    /// height with no block is skipped rather than aborting the range.
    pub async fn get_block_range_nullifiers(
        &self,
        range: HeightRange,
    ) -> Result<Vec<compact::CompactBlock>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let mut blocks = Vec::new();
        for raw_height in u32::from(range.start)..=u32::from(range.end) {
            // The bound is a valid `Height` and `raw_height` stays within it, so
            // the conversion cannot fail; classify a would-be failure as internal
            // rather than panic.
            let height = Height::try_from(raw_height)
                .map_err(|e| ServeError::Internal(format!("height in range invalid: {e}")))?;
            if let Some(block) = snapshot
                .compact_block_nullifiers(BlockRef::Height(height))
                .await?
            {
                blocks.push(block.to_wire());
            }
        }
        Ok(blocks)
    }

    /// `SendTransaction`: relay raw bytes. A rejection is a domain answer, so it
    /// rides out in the `SendResponse` (non-zero `error_code`), not as an error.
    pub async fn send_transaction(&self, raw: proto::RawTransaction) -> proto::SendResponse {
        match self.engine.broadcast(raw.data.to_vec()).await {
            Ok(txid) => proto::SendResponse {
                error_code: 0,
                error_message: to_hex(txid.into()),
            },
            Err(rejection) => proto::SendResponse {
                error_code: -1,
                error_message: format!("{rejection:?}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LightServe, ServeError};
    use zaino_core::{BlockHash, BlockId, Height};
    use zaino_proto::proto::service as proto;
    use zaino_service::testing::{MockChain, MockIndexerService};

    fn engine_with_tip(tip: Option<BlockId>) -> MockIndexerService {
        MockIndexerService::new(MockChain {
            tip,
            ..Default::default()
        })
    }

    /// The handler binds only `LightServeService`, pins a snapshot, and converts
    /// the domain tip to a wire `BlockId`.
    #[tokio::test]
    async fn latest_block_maps_domain_to_wire() {
        let tip = BlockId {
            height: Height::try_from(808).expect("valid height"),
            hash: BlockHash::from([0xABu8; 32]),
        };
        let serve = LightServe::new(engine_with_tip(Some(tip)));

        let wire = serve.get_latest_block().await.expect("latest block");
        assert_eq!(wire.height, 808u64);
        assert_eq!(wire.hash, vec![0xABu8; 32]);
    }

    /// An empty chain is `NoBlocks` (a serviceability fact), not a transport error.
    #[tokio::test]
    async fn latest_block_no_tip_is_no_blocks() {
        let serve = LightServe::new(engine_with_tip(None));
        assert!(matches!(
            serve.get_latest_block().await,
            Err(ServeError::NoBlocks)
        ));
    }

    /// `GetLightdInfo` reports the tip height and succeeds once the chain has a
    /// tip — the readiness signal clients gate on.
    #[tokio::test]
    async fn lightd_info_reports_the_tip_height() {
        let tip = BlockId {
            height: Height::try_from(42).expect("valid height"),
            hash: BlockHash::from([0x11u8; 32]),
        };
        let serve = LightServe::new(engine_with_tip(Some(tip)));
        let info = serve.get_lightd_info().await.expect("lightd info");
        assert_eq!(info.block_height, 42u64);
        assert_eq!(info.estimated_height, 42u64);
        assert!(info.taddr_support);
        assert!(!info.version.is_empty());
    }

    /// Before any block is served, `GetLightdInfo` still succeeds with height 0
    /// (a valid answer, not `NoBlocks`) — the call itself is the liveness gate.
    #[tokio::test]
    async fn lightd_info_before_any_block_is_height_zero() {
        let serve = LightServe::new(engine_with_tip(None));
        let info = serve.get_lightd_info().await.expect("lightd info");
        assert_eq!(info.block_height, 0u64);
    }

    /// `GetTreeState` delegates to the snapshot's treestate read: the mock's
    /// stub answers `NotServiceable`, which passes through as the serviceability
    /// fact rather than the old `unimplemented` — proving the handler is wired to
    /// the read, not stubbed at the wire.
    #[tokio::test]
    async fn tree_state_delegates_to_the_snapshot_read() {
        use zaino_core::Height;
        let serve = LightServe::new(engine_with_tip(None));
        let height = Height::try_from(2_800_000).expect("valid height");
        assert!(matches!(
            serve.get_tree_state(height).await,
            Err(ServeError::NotServiceable(_))
        ));
    }

    /// `GetLatestTreeState` needs a tip to key the treestate on; an empty chain
    /// is `NoBlocks`, not a transport error.
    #[tokio::test]
    async fn latest_tree_state_no_tip_is_no_blocks() {
        let serve = LightServe::new(engine_with_tip(None));
        assert!(matches!(
            serve.get_latest_tree_state().await,
            Err(ServeError::NoBlocks)
        ));
    }

    /// `GetSubtreeRoots` delegates to the snapshot and converts the result to
    /// wire — the mock serves an empty run, which returns an empty wire vec (a
    /// served answer, not `unimplemented`).
    #[tokio::test]
    async fn subtree_roots_delegates_and_converts() {
        use zaino_core::ShieldedPool;
        let serve = LightServe::new(engine_with_tip(None));
        let roots = serve
            .get_subtree_roots(ShieldedPool::Sapling, 0, None)
            .await
            .expect("subtree roots served");
        assert!(roots.is_empty());
    }

    /// `GetTransaction` delegates to the raw-transaction read: the mock answers a
    /// domain miss (`Ok(None)`), which passes through as not-found, proving the
    /// handler is wired to the read rather than stubbed at the wire.
    #[tokio::test]
    async fn get_transaction_delegates_to_the_snapshot_read() {
        use zaino_core::TransactionId;
        let serve = LightServe::new(engine_with_tip(None));
        let tx = serve
            .get_transaction(TransactionId::from([0x33u8; 32]))
            .await
            .expect("served");
        assert!(tx.is_none());
    }

    /// `GetTaddressBalance` needs a tip to bound the range; with one, it delegates
    /// to the address balance read (the mock reports `NotServiceable`, surfacing
    /// as the serviceability fact, not the old `unimplemented`).
    #[tokio::test]
    async fn taddress_balance_delegates_over_the_indexed_range() {
        use zaino_core::{BlockHash, BlockId, Height, TransparentAddress};
        let tip = BlockId {
            height: Height::try_from(500).expect("valid height"),
            hash: BlockHash::from([0x44u8; 32]),
        };
        let serve = LightServe::new(engine_with_tip(Some(tip)));
        assert!(matches!(
            serve
                .get_taddress_balance(vec![TransparentAddress::new("t1probe".to_string())])
                .await,
            Err(ServeError::NotServiceable(_))
        ));
    }

    /// An empty chain has no tip to bound the balance range, so it is `NoBlocks`.
    #[tokio::test]
    async fn taddress_balance_no_tip_is_no_blocks() {
        use zaino_core::TransparentAddress;
        let serve = LightServe::new(engine_with_tip(None));
        assert!(matches!(
            serve
                .get_taddress_balance(vec![TransparentAddress::new("t1probe".to_string())])
                .await,
            Err(ServeError::NoBlocks)
        ));
    }

    /// `GetAddressUtxos` delegates to the unspent-outpoints read (the mock serves
    /// an empty set) and returns an empty wire list — a served answer.
    #[tokio::test]
    async fn address_utxos_delegates_and_converts() {
        use zaino_core::{Height, TransparentAddress};
        let serve = LightServe::new(engine_with_tip(None));
        let utxos = serve
            .get_address_utxos(
                vec![TransparentAddress::new("t1probe".to_string())],
                Height::GENESIS,
                0,
            )
            .await
            .expect("served");
        assert!(utxos.is_empty());
    }

    /// `GetTaddressTxids` delegates to the address txid read (the mock serves an
    /// empty run) and returns no transactions — a served answer.
    #[tokio::test]
    async fn taddress_txids_delegates_and_converts() {
        use zaino_core::{Height, HeightRange, TransparentAddress};
        let serve = LightServe::new(engine_with_tip(None));
        let range = HeightRange {
            start: Height::GENESIS,
            end: Height::try_from(100).expect("valid height"),
        };
        let txs = serve
            .get_taddress_txids(TransparentAddress::new("t1probe".to_string()), range)
            .await
            .expect("served");
        assert!(txs.is_empty());
    }

    /// `GetBlockNullifiers` delegates to the nullifier read (the mock has no block
    /// there) and returns `None` — a served answer, not `unimplemented`.
    #[tokio::test]
    async fn block_nullifiers_delegates_to_the_snapshot_read() {
        use zaino_core::{BlockRef, Height};
        let serve = LightServe::new(engine_with_tip(None));
        let block = serve
            .get_block_nullifiers(BlockRef::Height(Height::try_from(10).expect("valid height")))
            .await
            .expect("served");
        assert!(block.is_none());
    }

    /// A successful broadcast returns `error_code == 0` with the txid in hex.
    #[tokio::test]
    async fn send_transaction_success_is_a_response() {
        let serve = LightServe::new(engine_with_tip(None));
        let resp = serve
            .send_transaction(proto::RawTransaction {
                data: vec![1, 2, 3].into(),
                height: 0,
            })
            .await;
        assert_eq!(resp.error_code, 0);
        assert_eq!(resp.error_message, "0".repeat(64)); // mock returns the zero txid
    }
}
