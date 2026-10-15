//! `zaino-lightserve` — POC lightwalletd-compatible gRPC serve adapter.
//!
//! Proof that the new trait algebra rebinds the serving layer cleanly. A handler
//! bound to [`LightWalletService`] alone (not the god-traits) pins a snapshot,
//! reads domain types, converts **domain -> wire in the adapter** (see
//! the `wire` module), and maps errors by kind — a transient snapshot failure, a
//! not-yet-serviceable chain, and a domain broadcast rejection are three
//! different wire outcomes, not one fused transport error.
//!
//! The handler covers the full light-wallet read-set — compact blocks, treestate
//! and subtree roots, transactions, transparent-address reads, the
//! nullifier-populated variants — plus `GetLightdInfo`, `SendTransaction`, and the
//! mempool passthrough (`GetMempoolTx` / `GetMempoolStream`). [`GrpcServer`]
//! stands up a real tonic `CompactTxStreamer` server over it
//! ([`RunLoop`](zaino_component::RunLoop)).
#![forbid(unsafe_code)]

#[cfg(test)]
mod bench_compact_block;
#[cfg(feature = "prometheus")]
mod emit;
mod error;
mod grpc;
#[cfg(feature = "prometheus")]
mod observe;
mod range;
mod schedule;
mod transport;
mod wire;

#[cfg(feature = "prometheus")]
pub use emit::{describe_metrics, METRIC_BUCKETS};
pub use error::ServeError;
pub use grpc::GrpcService;
pub use transport::{GrpcServeError, GrpcServer};

use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt};
use zaino_primitives::types::{
    BlockSelector, Height, HeightRange, NetworkUpgradeInfo, RawTransaction, ShieldedPool,
    TransactionId, TransactionLocation, TransparentAddress,
};
use zaino_proto::proto::compact_formats as compact;
use zaino_proto::proto::service as proto;
use zaino_service::queries;
use zaino_service::MempoolTx;
use zaino_service::{
    AddressRead, ChainSegment, CompactBlockRead, CompactNullifierRead, LightWalletService,
    RawTransactionRead, ReadBudget, TreestateRead,
};

use zcash_protocol::consensus::Network;

use crate::wire::{
    bip70_network_name, compact_tx_to_wire, to_hex, treestate_to_wire, zat_to_i64, ToWire,
};

/// Whether a mempool txid should be excluded, given the request's suffix list.
/// A suffix matches when it is a (non-empty) suffix of the txid's 32 bytes — the
/// same byte order the served `CompactTx.txid` carries, so the caller's exclude
/// set and our output agree. An empty suffix is ignored (it would match every
/// transaction, which is never the intent).
fn txid_matches_a_suffix(txid: TransactionId, suffixes: &[Vec<u8>]) -> bool {
    let bytes = <[u8; 32]>::from(txid);
    suffixes
        .iter()
        .any(|suffix| !suffix.is_empty() && bytes.ends_with(suffix))
}

/// Lightwalletd-compatible handler over a [`LightWalletService`] engine.
///
/// Carries the network it serves and the validator's network upgrade schedule:
/// the lightwalletd `TreeState.network` and `GetLightdInfo` chain fields are a
/// function of the chain, not of any block read, so they are serving parameters
/// the adapter renders — not capabilities the engine answers.
#[derive(Clone)]
pub struct LightServe<S: LightWalletService> {
    engine: S,
    network: Network,
    upgrades: Arc<[NetworkUpgradeInfo]>,
}

impl<S: LightWalletService> LightServe<S> {
    /// Build the handler over `engine`, serving `network`'s chain names, with no
    /// upgrade schedule: `GetLightdInfo` then leaves its schedule fields empty.
    pub fn new(engine: S, network: Network) -> Self {
        Self {
            engine,
            network,
            upgrades: Arc::from([]),
        }
    }

    /// Carry the validator's network upgrade schedule, as read at boot, so
    /// `GetLightdInfo` reports the Sapling activation height, the consensus
    /// branch in force at the served tip, and the next pending upgrade.
    pub fn with_upgrades(mut self, upgrades: Vec<NetworkUpgradeInfo>) -> Self {
        self.upgrades = upgrades.into();
        self
    }

    /// `GetLatestBlock`: the tip of the pinned best chain, as a wire `BlockRef`.
    pub async fn get_latest_block(&self) -> Result<proto::BlockId, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(ServeError::NoBlocks)?;
        Ok(tip.to_wire())
    }

    /// `GetLightdInfo`: serving metadata + the current tip height.
    ///
    /// `version`/`vendor`/`taddr_support` are static, `chain_name` is the served
    /// network's BIP70 name (as zebra renders it), and
    /// `block_height`/`estimated_height` are read from the pinned tip (0 before
    /// any block is served). The schedule fields come from the upgrade schedule
    /// the handler carries ([`with_upgrades`](Self::with_upgrades)):
    /// `sapling_activation_height`, `consensus_branch_id` (the branch in force
    /// at the pinned tip, as lightwalletd reports `consensus.chaintip`), and
    /// `upgrade_name`/`upgrade_height` for the next pending upgrade. Without a
    /// schedule they ride out empty.
    pub async fn get_lightd_info(&self) -> Result<proto::LightdInfo, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().map(|tip| tip.height);
        let block_height = tip.map(u64::from).unwrap_or(0);
        let schedule = schedule::at(&self.upgrades, tip);
        Ok(proto::LightdInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            vendor: "zaino".to_string(),
            taddr_support: true,
            chain_name: bip70_network_name(self.network).to_string(),
            sapling_activation_height: schedule.sapling_activation.map(u64::from).unwrap_or(0),
            consensus_branch_id: schedule
                .branch_in_force
                .map(|branch| branch.to_string())
                .unwrap_or_default(),
            block_height,
            estimated_height: block_height,
            upgrade_name: schedule
                .next_upgrade
                .map(|upgrade| upgrade.name.clone())
                .unwrap_or_default(),
            upgrade_height: schedule
                .next_upgrade
                .map(|upgrade| u64::from(upgrade.activation_height))
                .unwrap_or(0),
            ..Default::default()
        })
    }

    /// `GetBlock`: the composed compact block at `at`, or `None` when no block
    /// is indexed there (the caller maps that to a not-found status). The
    /// snapshot pins the view; the compact block is composed on read and
    /// converted domain -> wire in the adapter.
    pub async fn get_block(
        &self,
        at: BlockSelector,
    ) -> Result<Option<compact::CompactBlock>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot.compact_block(at).await?.map(ToWire::to_wire))
    }

    /// `GetBlockRange`: the composed compact blocks over `range`, as a stream.
    ///
    /// The stream owns the pinned snapshot and reads the range in bounded steps
    /// ([`range::steps`]), so a wide range is never composed whole before its
    /// first block is sent. A reversed range (`start > end`) is served highest
    /// first, as lightwalletd does. A per-block read failure rides the stream,
    /// classified by kind, rather than aborting the pin. Acquiring the snapshot
    /// itself can still fail up front (transient).
    pub async fn get_block_range(
        &self,
        range: HeightRange,
    ) -> Result<BoxStream<'static, Result<compact::CompactBlock, ServeError>>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let (direction, steps) = range::steps(range);
        Ok(futures::stream::iter(steps)
            .then(move |step| {
                let snapshot = snapshot.clone();
                async move {
                    let blocks: Vec<Result<compact::CompactBlock, ServeError>> = snapshot
                        .stream_compact(step)
                        .map(|read| read.map(ToWire::to_wire).map_err(ServeError::from))
                        .collect()
                        .await;
                    futures::stream::iter(direction.order(blocks))
                }
            })
            .flatten()
            .boxed())
    }

    /// `GetTreeState`: the commitment tree state at `at`, converted domain ->
    /// wire here.
    ///
    /// The treestate read is height-addressed, so a hash is first resolved to
    /// its height on the pinned best chain, through the same snapshot the
    /// treestate is then read from. `None` when no block on the served chain
    /// has that hash (unknown, or on a competing branch).
    pub async fn get_tree_state(
        &self,
        at: BlockSelector,
    ) -> Result<Option<proto::TreeState>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let height = match at {
            BlockSelector::Height(height) => height,
            BlockSelector::Hash(_) => match snapshot.compact_block(at).await? {
                Some(block) => Height::try_from(block.height).map_err(|e| {
                    ServeError::Internal(format!("a served block's height is invalid: {e}"))
                })?,
                None => return Ok(None),
            },
        };
        Ok(Some(treestate_to_wire(
            snapshot.treestate(height).await?,
            self.network,
        )))
    }

    /// `GetLatestTreeState`: the tree state at the pinned tip. `NoBlocks` before
    /// any block is served (there is no tip to key the treestate on).
    pub async fn get_latest_tree_state(&self) -> Result<proto::TreeState, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(ServeError::NoBlocks)?;
        Ok(treestate_to_wire(
            snapshot.treestate(tip.height).await?,
            self.network,
        ))
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

    /// `GetTransaction`: a transaction as raw bytes with where it was mined, or
    /// `None` when the validator does not know it (the caller maps that to
    /// not-found). The bytes are the validator's; the height is the engine's,
    /// which under a local transaction-location placement is where the pinned
    /// view holds the transaction.
    pub async fn get_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<proto::RawTransaction>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot.raw_transaction(id).await?.map(ToWire::to_wire))
    }

    /// `GetTaddressBalance`: the total current balance across `addrs`, summed
    /// over the whole indexed chain (genesis .. tip). `NoBlocks` before any block
    /// is served. Each address balance is read where the deployment's routing
    /// places address reads: the local address index or the validator.
    pub async fn get_taddress_balance(
        &self,
        addrs: Vec<TransparentAddress>,
    ) -> Result<proto::Balance, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        // Wallet policy (an unserviceable snapshot must not read as zero) lives
        // in the shared query layer; this handler maps its `None` to the wire
        // error a light client expects and renders the domain answer.
        let total = queries::wallet_balance(&snapshot, &addrs)
            .await?
            .ok_or(ServeError::NoBlocks)?;
        Ok(proto::Balance {
            value_zat: zat_to_i64(total.balance.as_u64()),
        })
    }

    /// `GetAddressUtxos`: the unspent outputs at or above `start_height` across
    /// `addrs`, capped at `max_entries` (`0` meaning unlimited). Read where the
    /// deployment's routing places address reads (the local address index or the
    /// validator); collected owned so the gRPC layer can serve the list or a
    /// `'static` stream.
    pub async fn get_address_utxos(
        &self,
        addrs: Vec<TransparentAddress>,
        start_height: Height,
        max_entries: usize,
    ) -> Result<Vec<proto::GetAddressUtxosReply>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        // One budget spans the request, so the unspent sets read across every
        // address are bounded together rather than per address.
        let mut budget = ReadBudget::for_request();
        let mut utxos = Vec::new();
        for addr in &addrs {
            for utxo in snapshot.unspent_outpoints(addr, &mut budget).await? {
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
        let mut budget = ReadBudget::for_request();
        let ids = snapshot.tx_ids(&addr, range, &mut budget).await?;
        let mut txs = Vec::with_capacity(ids.len());
        // The read pairs each txid with where it touched the address (height and
        // in-block position); the light-wallet wire carries only the transaction
        // bytes, so both are dropped here.
        for (_height, _position, id) in ids {
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
        at: BlockSelector,
    ) -> Result<Option<compact::CompactBlock>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot
            .compact_block_nullifiers(at)
            .await?
            .map(ToWire::to_wire))
    }

    /// `GetBlockRangeNullifiers`: the nullifier-populated compact blocks over
    /// `range`, as a stream read in bounded steps like
    /// [`get_block_range`](Self::get_block_range). A height with no block is
    /// skipped rather than aborting the range; a read failure ends the stream
    /// with that error.
    pub async fn get_block_range_nullifiers(
        &self,
        range: HeightRange,
    ) -> Result<BoxStream<'static, Result<compact::CompactBlock, ServeError>>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let (direction, steps) = range::steps(range);
        Ok(futures::stream::iter(steps)
            .then(move |step| {
                let snapshot = snapshot.clone();
                async move {
                    let mut blocks = Vec::new();
                    for raw_height in u32::from(step.start)..=u32::from(step.end) {
                        let read = match Height::try_from(raw_height) {
                            // Within a step of valid heights, so always valid;
                            // classified as internal rather than a panic.
                            Err(e) => Err(ServeError::Internal(format!(
                                "height in range invalid: {e}"
                            ))),
                            Ok(height) => snapshot
                                .compact_block_nullifiers(BlockSelector::Height(height))
                                .await
                                .map_err(ServeError::from),
                        };
                        match read {
                            Ok(Some(block)) => blocks.push(Ok(block.to_wire())),
                            Ok(None) => {}
                            Err(error) => {
                                blocks.push(Err(error));
                                break;
                            }
                        }
                    }
                    futures::stream::iter(direction.order(blocks))
                }
            })
            .flatten()
            .boxed())
    }

    /// `GetMempoolTx`: the compact projections of the mempool transactions the
    /// caller does not already hold. Live passthrough: the txids come from the
    /// mempool subscription, each projected to compact by the same source; a txid
    /// the caller lists in `exclude_txid_suffixes` (matched against the txid's
    /// bytes) is skipped, as is one dropped between listing and fetch. The stream
    /// index is the transaction's position in the mempool listing. Pool-type
    /// pruning (the request's `shielded_protocol` filter) is a later refinement —
    /// the compact projection already carries only the shielded (and transparent)
    /// components a wallet scans.
    pub async fn get_mempool_tx(
        &self,
        exclude_txid_suffixes: Vec<Vec<u8>>,
    ) -> Result<Vec<compact::CompactTx>, ServeError> {
        let listing: Vec<MempoolTx> = self.engine.subscribe_mempool().collect().await;
        let mut txs = Vec::new();
        for (position, entry) in listing.into_iter().enumerate() {
            if txid_matches_a_suffix(entry.txid, &exclude_txid_suffixes) {
                continue;
            }
            if let Some(compact) = self.engine.mempool_compact_transaction(entry.txid).await? {
                let index = u64::try_from(position)
                    .map_err(|e| ServeError::Internal(format!("mempool index overflow: {e}")))?;
                txs.push(compact_tx_to_wire(index, compact));
            }
        }
        Ok(txs)
    }

    /// `GetMempoolStream`: the raw transactions currently in the mempool. Live
    /// passthrough — not pinned to a snapshot, because the mempool is not part of
    /// any chain view. The txids come from the mempool subscription and each is
    /// hydrated to raw bytes from the same source; a txid dropped between listing
    /// and fetch (a race) is skipped. Collected owned so the gRPC layer serves a
    /// `'static` stream.
    pub async fn get_mempool_stream(&self) -> Result<Vec<proto::RawTransaction>, ServeError> {
        let listing: Vec<MempoolTx> = self.engine.subscribe_mempool().collect().await;
        let mut txs = Vec::with_capacity(listing.len());
        for entry in listing {
            if let Some(data) = self.engine.mempool_raw_transaction(entry.txid).await? {
                txs.push(
                    RawTransaction {
                        data,
                        location: TransactionLocation::Mempool,
                    }
                    .to_wire(),
                );
            }
        }
        Ok(txs)
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
    use zaino_primitives::types::{BlockHash, BlockRef, Height};
    use zaino_proto::proto::service as proto;
    use zaino_service::testing::{MockChain, MockIndexerService};
    use zcash_protocol::consensus::Network;

    fn engine_with_tip(tip: Option<BlockRef>) -> MockIndexerService {
        MockIndexerService::new(MockChain {
            tip,
            ..Default::default()
        })
    }

    /// A mainnet handler over `engine`. The network governs only the
    /// chain-name/`network` wire fields; tests that pin those assert the name
    /// directly, and the rest serve mainnet for a concrete handler.
    fn serve(engine: MockIndexerService) -> LightServe<MockIndexerService> {
        LightServe::new(engine, Network::MainNetwork)
    }

    /// The handler binds only `LightWalletService`, pins a snapshot, and converts
    /// the domain tip to a wire `BlockRef`.
    #[tokio::test]
    async fn latest_block_maps_domain_to_wire() {
        let tip = BlockRef {
            height: Height::try_from(808).expect("valid height"),
            hash: BlockHash::from([0xABu8; 32]),
        };
        let serve = serve(engine_with_tip(Some(tip)));

        let wire = serve.get_latest_block().await.expect("latest block");
        assert_eq!(wire.height, 808u64);
        assert_eq!(wire.hash, vec![0xABu8; 32]);
    }

    /// An empty chain is `NoBlocks` (a serviceability fact), not a transport error.
    #[tokio::test]
    async fn latest_block_no_tip_is_no_blocks() {
        let serve = serve(engine_with_tip(None));
        assert!(matches!(
            serve.get_latest_block().await,
            Err(ServeError::NoBlocks)
        ));
    }

    /// `GetLightdInfo` reports the tip height and succeeds once the chain has a
    /// tip — the readiness signal clients gate on.
    #[tokio::test]
    async fn lightd_info_reports_the_tip_height() {
        let tip = BlockRef {
            height: Height::try_from(42).expect("valid height"),
            hash: BlockHash::from([0x11u8; 32]),
        };
        let serve = serve(engine_with_tip(Some(tip)));
        let info = serve.get_lightd_info().await.expect("lightd info");
        assert_eq!(info.block_height, 42u64);
        assert_eq!(info.estimated_height, 42u64);
        assert!(info.taddr_support);
        assert!(!info.version.is_empty());
        // The served network's BIP70 name, as zebra/lightwalletd render it.
        assert_eq!(info.chain_name, "main");
    }

    /// Before any block is served, `GetLightdInfo` still succeeds with height 0
    /// (a valid answer, not `NoBlocks`) — the call itself is the liveness gate.
    #[tokio::test]
    async fn lightd_info_before_any_block_is_height_zero() {
        let serve = serve(engine_with_tip(None));
        let info = serve.get_lightd_info().await.expect("lightd info");
        assert_eq!(info.block_height, 0u64);
    }

    /// `GetTreeState` delegates to the snapshot's treestate read: the mock's
    /// stub answers `NotServiceable`, which passes through as the serviceability
    /// fact rather than the old `unimplemented` — proving the handler is wired to
    /// the read, not stubbed at the wire.
    #[tokio::test]
    async fn tree_state_delegates_to_the_snapshot_read() {
        use zaino_primitives::types::{BlockSelector, Height};
        let serve = serve(engine_with_tip(None));
        let height = Height::try_from(2_800_000).expect("valid height");
        assert!(matches!(
            serve.get_tree_state(BlockSelector::Height(height)).await,
            Err(ServeError::NotServiceable(_))
        ));
    }

    /// `GetLatestTreeState` needs a tip to key the treestate on; an empty chain
    /// is `NoBlocks`, not a transport error.
    #[tokio::test]
    async fn latest_tree_state_no_tip_is_no_blocks() {
        let serve = serve(engine_with_tip(None));
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
        use zaino_primitives::types::ShieldedPool;
        let serve = serve(engine_with_tip(None));
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
        use zaino_primitives::types::TransactionId;
        let serve = serve(engine_with_tip(None));
        let tx = serve
            .get_transaction(TransactionId::from([0x33u8; 32]))
            .await
            .expect("served");
        assert!(tx.is_none());
    }

    /// With a serviceable snapshot the handler delegates to the shared wallet
    /// query, sums the scripted balances through the checked path, and renders
    /// the total as the wire `value_zat`.
    #[tokio::test]
    async fn taddress_balance_renders_the_summed_scripted_balance() {
        use zaino_primitives::types::{
            AddressBalance, BlockHash, BlockRef, Height, TransparentAddress, Zatoshis,
            ZatoshisFlowSum,
        };
        fn scripted(name: &str, zats: u64, received: u64) -> (String, AddressBalance) {
            (
                name.to_string(),
                AddressBalance {
                    balance: Zatoshis::new(zats).expect("valid amount"),
                    received: ZatoshisFlowSum::from_summed(received),
                },
            )
        }
        let serve = serve(MockIndexerService::new(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(500).expect("valid height"),
                hash: BlockHash::from([0x44u8; 32]),
            }),
            balances: vec![scripted("t1a", 500, 900), scripted("t1b", 250, 400)],
            ..Default::default()
        }));
        let balance = serve
            .get_taddress_balance(vec![
                TransparentAddress::new("t1a".to_string()),
                TransparentAddress::new("t1b".to_string()),
            ])
            .await
            .expect("served balance");
        assert_eq!(balance.value_zat, 750);
    }

    /// An empty chain has no tip to bound the balance range, so it is `NoBlocks`.
    #[tokio::test]
    async fn taddress_balance_no_tip_is_no_blocks() {
        use zaino_primitives::types::TransparentAddress;
        let serve = serve(engine_with_tip(None));
        assert!(matches!(
            serve
                .get_taddress_balance(vec![TransparentAddress::new("t1probe".to_string())])
                .await,
            Err(ServeError::NoBlocks)
        ));
    }

    /// A wallet must not be told "zero" by an indexer that cannot answer — even
    /// for an empty address list, the unserviceable snapshot wins over the
    /// empty-list-is-zero rule and the handler maps it to `NoBlocks`.
    #[tokio::test]
    async fn taddress_balance_errors_when_nothing_is_serviceable() {
        let serve = serve(MockIndexerService::new(MockChain::default()));
        assert!(matches!(
            serve.get_taddress_balance(Vec::new()).await,
            Err(ServeError::NoBlocks)
        ));
    }

    /// `GetAddressUtxos` delegates to the unspent-outpoints read (the mock serves
    /// an empty set) and returns an empty wire list — a served answer.
    #[tokio::test]
    async fn address_utxos_delegates_and_converts() {
        use zaino_primitives::types::{Height, TransparentAddress};
        let serve = serve(engine_with_tip(None));
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
        use zaino_primitives::types::{Height, HeightRange, TransparentAddress};
        let serve = serve(engine_with_tip(None));
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
        use zaino_primitives::types::{BlockSelector, Height};
        let serve = serve(engine_with_tip(None));
        let block = serve
            .get_block_nullifiers(BlockSelector::Height(
                Height::try_from(10).expect("valid height"),
            ))
            .await
            .expect("served");
        assert!(block.is_none());
    }

    /// `GetMempoolStream` delegates to the mempool subscription and per-txid
    /// hydration (the mock serves an empty mempool) and returns no transactions —
    /// a served answer, not `unimplemented`.
    #[tokio::test]
    async fn mempool_stream_delegates_and_serves() {
        let serve = serve(engine_with_tip(None));
        let txs = serve.get_mempool_stream().await.expect("served");
        assert!(txs.is_empty());
    }

    /// `GetMempoolTx` delegates to the subscription and the compact hydration (the
    /// mock serves an empty mempool) and returns no transactions — a served
    /// answer, not `unimplemented`. The exclude set is accepted.
    #[tokio::test]
    async fn mempool_tx_delegates_and_serves() {
        let serve = serve(engine_with_tip(None));
        let txs = serve
            .get_mempool_tx(vec![vec![0xAAu8; 4]])
            .await
            .expect("served");
        assert!(txs.is_empty());
    }

    /// The exclude matcher: a non-empty suffix of the txid's bytes excludes it; a
    /// non-matching suffix does not; an empty suffix never excludes.
    #[test]
    fn exclude_matches_a_txid_suffix() {
        use super::txid_matches_a_suffix;
        use zaino_primitives::types::TransactionId;
        let mut bytes = [0u8; 32];
        bytes[30] = 0xBE;
        bytes[31] = 0xEF;
        let txid = TransactionId::from(bytes);
        assert!(txid_matches_a_suffix(txid, &[vec![0xBE, 0xEF]]));
        assert!(!txid_matches_a_suffix(txid, &[vec![0xDE, 0xAD]]));
        assert!(!txid_matches_a_suffix(txid, &[Vec::new()]));
        assert!(!txid_matches_a_suffix(txid, &[]));
    }

    /// A successful broadcast returns `error_code == 0` with the txid in hex.
    #[tokio::test]
    async fn send_transaction_success_is_a_response() {
        let serve = serve(engine_with_tip(None));
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
