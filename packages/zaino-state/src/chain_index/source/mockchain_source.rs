//! Mock BlockchainSourceResult implementation.

use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};
use zaino_common::network::ActivationHeights;
use zebra_chain::{block::Block, orchard::tree as orchard, sapling::tree as sapling};
use zebra_chain::{
    block::Height,
    parameters::NetworkKind,
    serialization::ZcashSerialize as _,
    transparent::{Address, OutPoint, Output},
};
use zebra_rpc::methods::ValidateAddresses as _;

/// Build the txid → (height, tx) lookup map used by
/// [`MockchainSource::get_transaction`].
///
/// Each tx's `hash()` is computed once here (cryptographic cost) and
/// cached for the lifetime of the `MockchainSource`. First occurrence
/// wins if the same txid appears at multiple heights — matches the
/// original linear-scan behaviour (return on first match starting at
/// height 0).
fn build_txid_index(
    blocks: &[Arc<Block>],
) -> Arc<HashMap<zebra_chain::transaction::Hash, (usize, Arc<zebra_chain::transaction::Transaction>)>>
{
    let mut index = HashMap::new();
    for (height, block) in blocks.iter().enumerate() {
        for tx in &block.transactions {
            index
                .entry(tx.hash())
                .or_insert_with(|| (height, Arc::clone(tx)));
        }
    }
    Arc::new(index)
}

/// Transparent output data needed to answer address-index RPCs from mock chain blocks.
#[derive(Clone)]
struct MatchingTransparentOutput {
    /// Address matched by the output lock script.
    address: Address,
    /// Transaction hash containing the matched output.
    transaction_hash: zebra_chain::transaction::Hash,
    /// Output index within the transaction.
    output_index: u32,
    /// Full transparent output.
    output: Output,
    /// Block height containing the transaction.
    height: Height,
    /// Transaction index within the block.
    transaction_index: u32,
}

/// Normalizes a transparent address for matching against outputs on `network`.
///
/// Regtest and testnet share transparent address prefixes, so regtest
/// transparent addresses are normalized to `network.t_addr_kind()`.
/// Mainnet addresses are only matched on mainnet.
fn normalize_transparent_address_for_network(
    address: &Address,
    network: &zebra_chain::parameters::Network,
) -> Option<Address> {
    let network_kind = address.network_kind();
    let target_transparent_address_kind = network.t_addr_kind();

    match network.kind() {
        NetworkKind::Mainnet if network_kind != NetworkKind::Mainnet => return None,
        NetworkKind::Testnet | NetworkKind::Regtest
            if network_kind != NetworkKind::Testnet && network_kind != NetworkKind::Regtest =>
        {
            return None;
        }
        _ => {}
    }

    match address {
        Address::PayToPublicKeyHash { pub_key_hash, .. } => Some(Address::from_pub_key_hash(
            target_transparent_address_kind,
            *pub_key_hash,
        )),
        Address::PayToScriptHash { script_hash, .. } => Some(Address::from_script_hash(
            target_transparent_address_kind,
            *script_hash,
        )),
        Address::Tex { .. } => None,
    }
}

/// Returns the output address if it is one of the requested transparent addresses.
fn matching_output_address(
    output: &Output,
    requested_addresses: &HashSet<Address>,
    network: &zebra_chain::parameters::Network,
) -> Option<Address> {
    let output_address = output.address(network)?;

    if requested_addresses.contains(&output_address) {
        Some(output_address)
    } else {
        None
    }
}

/// Normalizes all requested transparent addresses for matching on the mock chain network.
fn normalize_requested_addresses_for_network(
    addresses: &HashSet<Address>,
    network: &zebra_chain::parameters::Network,
) -> HashSet<Address> {
    addresses
        .iter()
        .filter_map(|address| normalize_transparent_address_for_network(address, network))
        .collect()
}

/// Returns the Zebra network used by this static mock chain.
///
/// The mock chain data is generated from a regtest chain. Regtest uses testnet
/// transparent address prefixes, so output-derived transparent addresses use
/// `NetworkKind::Testnet`.
pub(crate) fn mockchain_network() -> zebra_chain::parameters::Network {
    ActivationHeights::default().to_regtest_network()
}

/// A test-only mock implementation of BlockchainReader using ordered lists by height.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub(crate) struct MockchainSource {
    blocks: Vec<Arc<Block>>,
    roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
    treestates: Vec<(Vec<u8>, Vec<u8>)>,
    hashes: Vec<BlockHash>,
    /// txid → (block index, tx). Built once at construction; lets
    /// `get_transaction` run in O(1) instead of scanning every tx.
    /// Wrapped in `Arc` so cloning a `MockchainSource` is cheap.
    txid_index: Arc<
        HashMap<
            zebra_chain::transaction::Hash,
            (usize, Arc<zebra_chain::transaction::Transaction>),
        >,
    >,
    active_chain_height: Arc<AtomicU32>,
    force_requests_against_source_to_fail: Arc<std::sync::atomic::AtomicBool>,
    /// One-shot test hook: fires on the first `get_block(HashOrHeight::Height(_))`
    /// call after [`Self::arm_one_shot_get_block_hook`], regardless of which
    /// height is requested. Used by race regression tests (#1126) to inject
    /// a `mine_blocks` mid-iter, deterministically placing the iter into the
    /// race window. Cleared after firing; subsequent `get_block` calls run
    /// unaffected.
    get_block_hook: Arc<Mutex<Option<Box<dyn FnOnce() + Send + Sync>>>>,
    /// Announces "blocks received" — i.e. [`Self::mine_blocks`] advanced
    /// the active height — to every subscriber registered via
    /// [`BlockchainSource::subscribe_to_blocks_received`], so each can
    /// wake from its interval timer immediately.
    ///
    /// Backed by `tokio::sync::watch`, the idiomatic Tokio primitive for
    /// "wake multiple subscribers when state has changed since they last
    /// looked." `send_replace(())` always triggers `changed()` on every
    /// receiver; multiple `send_replace` calls between two
    /// `changed().await` calls coalesce into a single wake by
    /// construction. The wake is a "something happened" signal — the
    /// subsystem re-reads source state on each wake — so subscribers
    /// neither know nor care how many `mine_blocks` events occurred
    /// between wakes.
    blocks_received_broadcaster: tokio::sync::watch::Sender<()>,
    /// Records whether [`BlockchainSource::shutdown`] ran, so teardown tests
    /// can assert the index releases its source. Shared across clones.
    shutdown_called: Arc<std::sync::atomic::AtomicBool>,
}

impl MockchainSource {
    /// Creates a new MockchainSource with `active_chain_height` set to
    /// the loaded chain's tip — every loaded block is immediately served.
    /// All inputs must be the same length, and ordered by ascending
    /// height starting from 0.
    #[allow(clippy::type_complexity)]
    pub(crate) fn new(
        blocks: Vec<Arc<Block>>,
        roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
        treestates: Vec<(Vec<u8>, Vec<u8>)>,
        hashes: Vec<BlockHash>,
    ) -> Self {
        // len() returns one-indexed length, height is zero-indexed.
        let tip_height = blocks.len().saturating_sub(1) as u32;
        Self::new_with_active_height(blocks, roots, treestates, hashes, tip_height)
    }

    /// Creates a new MockchainSource, *with* an active chain height.
    ///
    /// Block will only be served up to the active chain height, with mempool data coming from
    /// the *next block in the chain.
    ///
    /// Blocks must be "mined" to extend the active chain height.
    ///
    /// All inputs must be the same length, and ordered by ascending height starting from 0.
    #[allow(clippy::type_complexity)]
    pub(crate) fn new_with_active_height(
        blocks: Vec<Arc<Block>>,
        roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
        treestates: Vec<(Vec<u8>, Vec<u8>)>,
        hashes: Vec<BlockHash>,
        active_chain_height: u32,
    ) -> Self {
        assert!(
            blocks.len() == roots.len()
                && roots.len() == hashes.len()
                && hashes.len() == treestates.len(),
            "All input vectors must be the same length"
        );
        assert!(
            !blocks.is_empty(),
            "MockchainSource requires at least a genesis block"
        );

        // len() returns one-indexed length, height is zero-indexed.
        let max_height = blocks.len().saturating_sub(1) as u32;
        assert!(
            active_chain_height <= max_height,
            "active_chain_height must be in 0..=len-1"
        );

        let txid_index = build_txid_index(&blocks);
        Self {
            blocks,
            roots,
            treestates,
            hashes,
            txid_index,
            active_chain_height: Arc::new(AtomicU32::new(active_chain_height)),
            force_requests_against_source_to_fail: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            get_block_hook: Arc::new(Mutex::new(None)),
            blocks_received_broadcaster: tokio::sync::watch::channel(()).0,
            shutdown_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Whether [`BlockchainSource::shutdown`] has run on this source (or any
    /// clone of it).
    pub(crate) fn shutdown_called(&self) -> bool {
        self.shutdown_called.load(Ordering::SeqCst)
    }

    /// When set to true, `get_best_block_height` and `get_best_block_hash`
    /// return `BlockchainSourceError::Unrecoverable`.
    pub(crate) fn set_failing(&self, fail: bool) {
        self.force_requests_against_source_to_fail
            .store(fail, Ordering::SeqCst);
    }

    /// Advances `active_chain_height` by up to `blocks`, capped at
    /// `max_chain_height`. Returns `true` iff the height changed; on a
    /// no-op advance (already at the cap) returns `false` so callers
    /// can decide whether to fire the change-notify.
    fn advance_active_height(&self, blocks: u32) -> bool {
        // len() returns one-indexed length, height is zero-indexed.
        let max_height = self.max_chain_height();
        self.active_chain_height
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                let target = current.saturating_add(blocks).min(max_height);
                if target == current {
                    None
                } else {
                    Some(target)
                }
            })
            .is_ok()
    }

    pub(crate) fn mine_blocks(&self, blocks: u32) {
        if self.advance_active_height(blocks) {
            self.blocks_received_broadcaster.send_replace(());
        }
    }

    /// Like [`Self::mine_blocks`] but does *not* fire the source's
    /// change-notify. Lets the chain-index sync loop fall through to its
    /// timer instead of waking immediately — the only way to put the
    /// chain-index *behind* the mempool in tests, since the mempool's
    /// serve loop polls `get_best_block_hash` directly and always
    /// notices, notify or not.
    pub(crate) fn mine_blocks_silent(&self, blocks: u32) {
        self.advance_active_height(blocks);
    }

    /// Arm a one-shot hook that fires the next time
    /// `get_block(HashOrHeight::Height(_))` is called, before the source
    /// checks its active height. Used by race regression tests (#1126) to
    /// inject a mid-iter source advance at a precise point — when the
    /// worker's height-keyed fetch path is about to fetch its first block
    /// of the iter, regardless of which specific height it requests first.
    ///
    /// The closure runs synchronously inside `get_block`; do non-blocking
    /// work only (e.g. [`Self::mine_blocks`]). The hook is cleared after
    /// firing; replacing an armed hook is a silent overwrite.
    pub(crate) fn arm_one_shot_get_block_hook(&self, f: Box<dyn FnOnce() + Send + Sync>) {
        *self
            .get_block_hook
            .lock()
            .expect("get_block_hook mutex poisoned") = Some(f);
    }

    pub(crate) fn max_chain_height(&self) -> u32 {
        // len() returns one-indexed length, height is zero-indexed.
        self.blocks.len().saturating_sub(1) as u32
    }

    pub(crate) fn active_height(&self) -> u32 {
        self.active_chain_height.load(Ordering::SeqCst)
    }

    fn valid_height(&self, height: u32) -> Option<usize> {
        let active_chain_height = self.active_height() as usize;
        let valid_height = height as usize;

        if valid_height <= active_chain_height {
            Some(valid_height)
        } else {
            None
        }
    }

    fn valid_hash(&self, hash: &zebra_chain::block::Hash) -> Option<usize> {
        let active_chain_height = self.active_height() as usize;
        let height_index = self.hashes.iter().position(|h| h.0 == hash.0);

        if height_index.is_some() && height_index.unwrap() <= active_chain_height {
            height_index
        } else {
            None
        }
    }

    fn active_chain_height_as_usize(&self) -> usize {
        self.active_height() as usize
    }

    /// The zebra hash of the block after `height_index`, if it is within the active chain.
    fn next_block_hash(&self, height_index: usize) -> Option<zebra_chain::block::Hash> {
        let next = height_index + 1;
        (next <= self.active_chain_height_as_usize()).then(|| self.blocks[next].hash())
    }

    fn block_height_at_index(&self, block_index: usize) -> Height {
        self.blocks[block_index]
            .coinbase_height()
            .unwrap_or(Height(block_index as u32))
    }

    fn matching_transparent_outputs(
        &self,
        addresses: &HashSet<Address>,
        network: &zebra_chain::parameters::Network,
    ) -> HashMap<OutPoint, MatchingTransparentOutput> {
        let requested_addresses = normalize_requested_addresses_for_network(addresses, network);
        let mut matching_outputs = HashMap::new();
        let active_chain_height = self.active_chain_height_as_usize();

        if requested_addresses.is_empty() {
            return matching_outputs;
        }

        for block_index in 0..=active_chain_height {
            let block = &self.blocks[block_index];
            let height = self.block_height_at_index(block_index);

            for (transaction_index, transaction) in block.transactions.iter().enumerate() {
                let transaction_hash = transaction.hash();

                for (output_index, output) in transaction.outputs().iter().enumerate() {
                    let Some(address) =
                        matching_output_address(output, &requested_addresses, network)
                    else {
                        continue;
                    };

                    let outpoint = OutPoint::from_usize(transaction_hash, output_index);

                    matching_outputs.insert(
                        outpoint,
                        MatchingTransparentOutput {
                            address,
                            transaction_hash,
                            output_index: output_index as u32,
                            output: output.clone(),
                            height,
                            transaction_index: transaction_index as u32,
                        },
                    );
                }
            }
        }

        matching_outputs
    }

    fn spent_transparent_outpoints(&self) -> HashSet<OutPoint> {
        let mut spent_outpoints = HashSet::new();
        let active_chain_height = self.active_chain_height_as_usize();

        for block_index in 0..=active_chain_height {
            for transaction in &self.blocks[block_index].transactions {
                spent_outpoints.extend(transaction.spent_outpoints());
            }
        }

        spent_outpoints
    }

    fn transaction_touches_addresses(
        &self,
        transaction: &zebra_chain::transaction::Transaction,
        requested_addresses: &HashSet<Address>,
        matching_outputs: &HashMap<OutPoint, MatchingTransparentOutput>,
        network: &zebra_chain::parameters::Network,
    ) -> bool {
        transaction
            .outputs()
            .iter()
            .any(|output| matching_output_address(output, requested_addresses, network).is_some())
            || transaction
                .spent_outpoints()
                .any(|outpoint| matching_outputs.contains_key(&outpoint))
    }
}

// ---------------------------------------------------------------------------
// Response builders
//
// These moved here with the deletion of the old validator connector, which was their
// only other caller. They build the wire shapes the scaffolding port still
// returns, from a mock chain's own blocks.
//
// They go away with this mock: once it moves to `zaino-source` and implements
// the per-question ports, it answers in domain types and has no wire shapes to
// build.
// ---------------------------------------------------------------------------

/// Confirmations are one more than the depth, or -1 when the block is not on the best
/// chain. Depth is limited by height, so it never overflows an `i64`.
fn confirmations_from_depth(depth: Option<u32>) -> i64 {
    const NOT_IN_BEST_CHAIN_CONFIRMATIONS: i64 = -1;
    depth
        .map(|depth| i64::from(depth) + 1)
        .unwrap_or(NOT_IN_BEST_CHAIN_CONFIRMATIONS)
}

// ***** zaino-source port implementations *****
//
// The mock answers the same questions a validator does, in the domain
// vocabulary, and `ValidatorSource` converts them to the wire shapes ChainIndex
// consumes. Everything the mock cannot answer without richer test vectors stays
// `unimplemented!`, exactly as it was on `BlockchainSource` — the panic message
// is the record of what the vectors would have to carry.

use zaino_primitives::types as domain;
use zaino_source::{FailureMode, FetchError, QueryError as PortError};

/// A fixture failure, in the shape a port reports faults.
///
/// Both test fixtures share this: their failures are all "this fixture cannot
/// answer that", which has no domain meaning — it is closer to a transport
/// fault than to a validator rejecting a well-formed query.
pub(crate) fn port_fault<E: std::fmt::Debug + std::fmt::Display>(
    message: impl Into<String>,
) -> PortError<E> {
    PortError::Fetch(FetchError::new(FailureMode::Parse, message.into()))
}

impl MockchainSource {
    /// `Err` when a test has armed [`Self::set_failing`].
    fn forced_failure<E: std::fmt::Debug + std::fmt::Display>(&self) -> Option<PortError<E>> {
        self.force_requests_against_source_to_fail
            .load(Ordering::SeqCst)
            .then(|| port_fault("forced source failure"))
    }

    /// The index of a block served at this height, or `None` past the active tip.
    fn served_index_at_height(&self, height: domain::Height) -> Option<usize> {
        self.valid_height(u32::from(height))
    }

    /// The index of a block served under this hash, or `None` past the active tip.
    fn served_index_at_hash(&self, hash: domain::BlockHash) -> Option<usize> {
        let zebra_hash = zebra_chain::block::Hash(<[u8; 32]>::from(hash));
        self.valid_hash(&zebra_hash)
    }

    /// Serialize the block at an index, which every raw port returns.
    fn serialized_block_at(&self, index: usize) -> Result<Vec<u8>, String> {
        self.blocks[index]
            .zcash_serialize_to_vec()
            .map_err(|error| format!("mock block did not serialize: {error}"))
    }
}

impl zaino_source::GetRawBlock for MockchainSource {
    async fn get_raw_block(
        &self,
        height: domain::Height,
    ) -> Result<Vec<u8>, PortError<zaino_source::GetBlockError>> {
        // The one-shot hook fires before the active-height check, so a hook
        // that advances the active height is visible to this same call — the
        // race window the regression tests place themselves in.
        if let Some(hook) = self
            .get_block_hook
            .lock()
            .expect("get_block_hook mutex poisoned")
            .take()
        {
            hook();
        }

        let index = self
            .served_index_at_height(height)
            .ok_or(PortError::Domain(
                zaino_source::GetBlockError::HeightNotFound(height),
            ))?;
        self.serialized_block_at(index).map_err(port_fault)
    }
}

impl zaino_source::GetRawBlockByHash for MockchainSource {
    async fn get_raw_block_by_hash(
        &self,
        hash: domain::BlockHash,
    ) -> Result<Vec<u8>, PortError<zaino_source::GetBlockByHashError>> {
        let index = self.served_index_at_hash(hash).ok_or(PortError::Domain(
            zaino_source::GetBlockByHashError::NotFound(hash),
        ))?;
        self.serialized_block_at(index).map_err(port_fault)
    }
}

impl zaino_source::GetChainTip for MockchainSource {
    async fn get_chain_tip(
        &self,
    ) -> Result<(domain::BlockHash, domain::Height), PortError<zaino_source::GetChainTipError>>
    {
        if let Some(failure) = self.forced_failure() {
            return Err(failure);
        }
        let index = self.active_height() as usize;
        if self.blocks.is_empty() || index > self.max_chain_height() as usize {
            return Err(PortError::Domain(zaino_source::GetChainTipError::NotReady));
        }
        let block = &self.blocks[index];
        let height = block.coinbase_height().ok_or_else(|| {
            port_fault::<zaino_source::GetChainTipError>(format!(
                "mock block at index {index} has no coinbase height"
            ))
        })?;
        Ok((
            domain::BlockHash::from(block.hash().0),
            domain::Height::try_from(height.0)
                .map_err(|e| port_fault::<zaino_source::GetChainTipError>(e.to_string()))?,
        ))
    }
}

impl zaino_source::GetBestBlockHeight for MockchainSource {
    async fn get_best_block_height(
        &self,
    ) -> Result<domain::Height, PortError<zaino_source::GetBestBlockHeightError>> {
        if let Some(failure) = self.forced_failure() {
            return Err(failure);
        }
        let index = self.active_height() as usize;
        if self.blocks.is_empty() || index > self.max_chain_height() as usize {
            return Err(PortError::Domain(
                zaino_source::GetBestBlockHeightError::NotReady,
            ));
        }
        let height = self.blocks[index].coinbase_height().ok_or_else(|| {
            port_fault::<zaino_source::GetBestBlockHeightError>(format!(
                "active chain block at index {index} has no coinbase height"
            ))
        })?;
        domain::Height::try_from(height.0).map_err(|e| port_fault(e.to_string()))
    }
}

impl zaino_source::GetDifficulty for MockchainSource {
    async fn get_difficulty(
        &self,
    ) -> Result<domain::Difficulty, PortError<zaino_source::GetDifficultyError>> {
        let index = self.active_chain_height_as_usize();
        let block = self.blocks.get(index).ok_or_else(|| {
            port_fault::<zaino_source::GetDifficultyError>("mock chain has no tip block")
        })?;
        Ok(block
            .header
            .difficulty_threshold
            .relative_to_network(&mockchain_network()))
    }
}

impl MockchainSource {
    /// The mock's "mempool": the next block in the loaded chain, minus its
    /// coinbase — a transaction that cannot be in a mempool.
    ///
    /// One definition shared by all four mempool ports, so the txid listing, the
    /// verbose listing and the raw fetch cannot drift into disagreeing about
    /// what is in the mempool — which is exactly the incoherence the mempool
    /// subsystem's single-source rule exists to prevent, and would make the mock
    /// unable to exercise it.
    fn mempool_transactions(
        &self,
    ) -> impl Iterator<Item = &Arc<zebra_chain::transaction::Transaction>> {
        let mempool_index = self.active_height() as usize + 1;
        self.blocks
            .get(mempool_index)
            .into_iter()
            .flat_map(|block| block.transactions.iter())
            .filter(|transaction| !transaction.is_coinbase())
    }
}

impl zaino_source::GetMempoolTxids for MockchainSource {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<domain::TransactionId>, PortError<zaino_source::GetMempoolTxidsError>> {
        Ok(self
            .mempool_transactions()
            .map(|transaction| domain::TransactionId::from(transaction.hash().0))
            .collect())
    }
}

impl zaino_source::GetMempoolMetadata for MockchainSource {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<zaino_source::MempoolTxMeta>, PortError<zaino_source::GetMempoolMetadataError>>
    {
        // Every mock mempool transaction entered at the current tip: the mock
        // has no arrival history, and the tip is the honest answer for a set
        // that is defined as "the block that would come next".
        let entry_height = domain::Height::try_from(self.active_height())
            .map_err(|e| port_fault::<zaino_source::GetMempoolMetadataError>(e.to_string()))?;

        Ok(self
            .mempool_transactions()
            .map(|transaction| zaino_source::MempoolTxMeta {
                txid: domain::TransactionId::from(transaction.hash().0),
                entry_height,
                // No arrival time to report rather than a fabricated one: the
                // `Option` exists for exactly this, and a synthetic timestamp
                // would give the admission tiebreak a fake ordering to sort on.
                entry_time: None,
            })
            .collect())
    }
}

impl zaino_source::GetRawMempoolTransaction for MockchainSource {
    async fn get_raw_mempool_transaction(
        &self,
        txid: domain::TransactionId,
    ) -> Result<Vec<u8>, PortError<zaino_source::GetRawMempoolTransactionError>> {
        let wanted = zebra_chain::transaction::Hash(<[u8; 32]>::from(txid));

        let transaction = self
            .mempool_transactions()
            .find(|transaction| transaction.hash() == wanted)
            .ok_or(PortError::Domain(
                zaino_source::GetRawMempoolTransactionError::NotFound(txid),
            ))?;

        transaction
            .zcash_serialize_to_vec()
            .map_err(|e| port_fault::<zaino_source::GetRawMempoolTransactionError>(e.to_string()))
    }
}

impl zaino_source::GetMempoolSourceTip for MockchainSource {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<(domain::BlockHash, domain::Height), PortError<std::convert::Infallible>> {
        // The mock serves its mempool and its tip from one place by
        // construction — `mempool_transactions` is defined relative to
        // `active_height` — so the single-source rule holds trivially here.
        // Routed through `GetChainTip` rather than duplicated so it stays that
        // way as the mock changes.
        use zaino_source::GetChainTip as _;

        // `GetChainTip` has a domain answer for "no tip yet"; this port has
        // none, by design (see `GetMempoolSourceTip`). Reported as a fault,
        // which is what it is here — a fixture that cannot answer.
        self.get_chain_tip().await.map_err(|e| match e {
            PortError::Domain(zaino_source::GetChainTipError::NotReady) => {
                port_fault("mockchain has no chain tip to serve the mempool")
            }
            PortError::Fetch(fetch) => PortError::Fetch(fetch),
        })
    }
}

impl zaino_source::SubscribeBlocks for MockchainSource {
    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.blocks_received_broadcaster.subscribe())
    }
}

impl zaino_source::SourceLifecycle for MockchainSource {
    fn shutdown(&self) {
        self.shutdown_called.store(true, Ordering::SeqCst);
    }
}

impl zaino_source::GetTransaction for MockchainSource {
    async fn get_transaction(
        &self,
        txid: domain::TransactionId,
    ) -> Result<zaino_source::TransactionResponse, PortError<zaino_source::GetTransactionError>>
    {
        let zebra_txid = zebra_chain::transaction::Hash(<[u8; 32]>::from(txid));
        let Some((block_index, transaction)) = self.txid_index.get(&zebra_txid) else {
            return Err(PortError::Domain(
                zaino_source::GetTransactionError::NotFound(txid),
            ));
        };

        // Past the active tip the transaction exists in the loaded vectors but
        // has not been mined yet, which the mock serves as its mempool.
        let location = if *block_index <= self.active_chain_height_as_usize() {
            let height = self.block_height_at_index(*block_index);
            domain::TransactionLocation::BestChain(
                domain::Height::try_from(height.0).map_err(|e| port_fault(e.to_string()))?,
            )
        } else if *block_index == self.active_chain_height_as_usize() + 1 {
            domain::TransactionLocation::Mempool
        } else {
            return Err(PortError::Domain(
                zaino_source::GetTransactionError::NotFound(txid),
            ));
        };

        let bytes = transaction
            .zcash_serialize_to_vec()
            .map_err(|error| port_fault(format!("mock transaction did not serialize: {error}")))?;

        Ok(zaino_source::TransactionResponse { bytes, location })
    }
}

impl zaino_source::GetCommitmentTreeRoots for MockchainSource {
    async fn get_commitment_tree_roots(
        &self,
        block: domain::BlockHash,
    ) -> Result<domain::TreeRoots, PortError<zaino_source::GetCommitmentTreeRootsError>> {
        let Some(index) = self.served_index_at_hash(block) else {
            // Absent rather than an error: the scaffolding reported an unknown
            // block as three empty slots.
            return Ok(domain::TreeRoots {
                sapling: None,
                orchard: None,
                ironwood: None,
            });
        };

        let (sapling, orchard) = self.roots[index];
        let info = |root: [u8; 32], size: u64| domain::TreeRootInfo {
            root: domain::TreeRoot::from(root),
            size,
        };

        Ok(domain::TreeRoots {
            sapling: sapling.map(|(root, size)| info(<[u8; 32]>::from(root), size)),
            orchard: orchard.map(|(root, size)| info(<[u8; 32]>::from(root), size)),
            // The test vectors carry no ironwood tree.
            ironwood: None,
        })
    }
}

impl zaino_source::GetBlockVerboseByHash for MockchainSource {
    async fn get_block_verbose_by_hash(
        &self,
        hash: domain::BlockHash,
    ) -> Result<domain::BlockVerbose, PortError<zaino_source::GetBlockVerboseError>> {
        let index = self
            .served_index_at_hash(hash)
            .ok_or_else(|| port_fault::<zaino_source::GetBlockVerboseError>("block not found"))?;
        let block = &self.blocks[index];
        let height = block.coinbase_height().ok_or_else(|| {
            port_fault::<zaino_source::GetBlockVerboseError>("block missing coinbase height")
        })?;

        let (_, orchard) = self.roots[index];
        let (sapling_size, orchard_size) = (
            self.roots[index].0.map(|(_, size)| size).unwrap_or(0),
            orchard.map(|(_, size)| size).unwrap_or(0),
        );

        Ok(domain::BlockVerbose {
            confirmations: confirmations_from_depth(self.active_height().checked_sub(height.0)),
            difficulty: block
                .header
                .difficulty_threshold
                .relative_to_network(&mockchain_network()),
            // The vectors carry no cumulative chain state.
            chainwork: None,
            chain_supply: None,
            value_pools: Vec::new(),
            tree_sizes: domain::BlockTreeSizes {
                sapling: sapling_size,
                orchard: orchard_size,
                ironwood: 0,
            },
            next_block_hash: self
                .next_block_hash(index)
                .map(|hash| domain::BlockHash::from(hash.0)),
        })
    }
}

impl zaino_source::GetRawBlockHeader for MockchainSource {
    async fn get_raw_block_header(
        &self,
        hash: domain::BlockHash,
    ) -> Result<Vec<u8>, PortError<zaino_source::GetBlockHeaderError>> {
        let index = self.served_index_at_hash(hash).ok_or_else(|| {
            port_fault::<zaino_source::GetBlockHeaderError>("block height not in best chain")
        })?;
        self.blocks[index]
            .header
            .zcash_serialize_to_vec()
            .map_err(|error| port_fault(format!("mock header did not serialize: {error}")))
    }
}

impl zaino_source::GetTreestateByHash for MockchainSource {
    async fn get_treestate_by_hash(
        &self,
        hash: domain::BlockHash,
    ) -> Result<domain::Treestate, PortError<zaino_source::GetTreestateByHashError>> {
        let Some(index) = self.served_index_at_hash(hash) else {
            return Err(PortError::Domain(
                zaino_source::GetTreestateByHashError::BlockNotFound(hash),
            ));
        };
        let (sapling, orchard) = &self.treestates[index];
        let block = &self.blocks[index];
        let height = block.coinbase_height().ok_or_else(|| {
            port_fault::<zaino_source::GetTreestateByHashError>("block missing coinbase height")
        })?;

        Ok(domain::Treestate {
            block_hash: hash,
            height: domain::Height::try_from(height.0).map_err(|e| port_fault(e.to_string()))?,
            time: block.header.time.timestamp() as u32,
            // The vectors carry trees but no roots, matching what both
            // production adapters answer with.
            sapling: Some(domain::PoolTreestate {
                final_root: None,
                final_state: sapling.clone(),
            }),
            orchard: Some(domain::PoolTreestate {
                final_root: None,
                final_state: orchard.clone(),
            }),
            // The test vectors carry no ironwood tree.
            ironwood: None,
        })
    }
}

impl zaino_source::GetSubtreeRoots for MockchainSource {
    async fn get_subtree_roots(
        &self,
        pool: domain::ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<domain::SubtreeRoot>, PortError<zaino_source::GetSubtreeRootsError>> {
        let requested_limit = limit.map(usize::from).unwrap_or(usize::MAX);
        if requested_limit == 0 {
            return Ok(Vec::new());
        }

        let mut roots: Vec<domain::SubtreeRoot> = Vec::new();
        fn subtree_root(
            root: [u8; 32],
            height: zebra_chain::block::Height,
        ) -> Result<domain::SubtreeRoot, PortError<zaino_source::GetSubtreeRootsError>> {
            Ok(domain::SubtreeRoot {
                root: domain::TreeRoot::from(root),
                end_height: domain::Height::try_from(height.0)
                    .map_err(|e| port_fault(e.to_string()))?,
            })
        }

        match pool {
            domain::ShieldedPool::Sapling => {
                let mut tree = sapling::NoteCommitmentTree::default();
                for block_index in 0..=self.active_chain_height_as_usize() {
                    let height = self.block_height_at_index(block_index);
                    for note_commitment in self.blocks[block_index].sapling_note_commitments() {
                        tree.append(*note_commitment).map_err(|error| {
                            port_fault::<zaino_source::GetSubtreeRootsError>(format!(
                                "could not append Sapling note commitment to tree: {error}"
                            ))
                        })?;
                        let Some((subtree_index, subtree_root_value)) =
                            tree.completed_subtree_index_and_root()
                        else {
                            continue;
                        };
                        if subtree_index.0 < start_index {
                            continue;
                        }
                        roots.push(subtree_root(subtree_root_value.to_bytes(), height)?);
                        if roots.len() == requested_limit {
                            return Ok(roots);
                        }
                    }
                }
            }
            domain::ShieldedPool::Orchard => {
                let mut tree = orchard::NoteCommitmentTree::default();
                for block_index in 0..=self.active_chain_height_as_usize() {
                    let height = self.block_height_at_index(block_index);
                    for note_commitment in self.blocks[block_index].orchard_note_commitments() {
                        tree.append(*note_commitment).map_err(|error| {
                            port_fault::<zaino_source::GetSubtreeRootsError>(format!(
                                "could not append Orchard note commitment to tree: {error}"
                            ))
                        })?;
                        let Some((subtree_index, subtree_root_value)) =
                            tree.completed_subtree_index_and_root()
                        else {
                            continue;
                        };
                        if subtree_index.0 < start_index {
                            continue;
                        }
                        roots.push(subtree_root(subtree_root_value.to_repr(), height)?);
                        if roots.len() == requested_limit {
                            return Ok(roots);
                        }
                    }
                }
            }
            // The test vectors carry no ironwood tree.
            _ => {}
        }

        Ok(roots)
    }
}

impl zaino_source::GetBlockHeader for MockchainSource {
    async fn get_block_header(
        &self,
        hash: domain::BlockHash,
    ) -> Result<domain::rpc::BlockHeaderVerbose, PortError<zaino_source::GetBlockHeaderError>> {
        let index = self.served_index_at_hash(hash).ok_or_else(|| {
            port_fault::<zaino_source::GetBlockHeaderError>("block height not in best chain")
        })?;
        let block = &self.blocks[index];
        let header = &block.header;
        let height = block.coinbase_height().ok_or_else(|| {
            port_fault::<zaino_source::GetBlockHeaderError>("block missing coinbase height")
        })?;
        let network = mockchain_network();

        Ok(domain::rpc::BlockHeaderVerbose {
            hash,
            confirmations: confirmations_from_depth(self.active_height().checked_sub(height.0)),
            height: domain::Height::try_from(height.0).map_err(|e| port_fault(e.to_string()))?,
            version: header.version,
            merkle_root: domain::MerkleRoot::from(header.merkle_root.0),
            time: header.time.timestamp() as u32,
            nonce: *header.nonce,
            solution: equihash_solution_bytes(&header.solution)
                .map_err(port_fault::<zaino_source::GetBlockHeaderError>)?,
            bits: u32::from_be_bytes(header.difficulty_threshold.bytes_in_display_order()),
            difficulty: header.difficulty_threshold.relative_to_network(&network),
            block_commitments: Some(domain::BlockCommitments::from(*header.commitment_bytes)),
            final_sapling_root: self.roots[index]
                .0
                .map(|(root, _)| domain::TreeRoot::from(<[u8; 32]>::from(root))),
            // The vectors carry no cumulative work.
            chainwork: None,
            previous_block_hash: Some(domain::BlockHash::from(header.previous_block_hash.0)),
            next_block_hash: self
                .next_block_hash(index)
                .map(|hash| domain::BlockHash::from(hash.0)),
        })
    }
}

impl zaino_source::GetChainTips for MockchainSource {
    async fn get_chain_tips(
        &self,
    ) -> Result<Vec<domain::rpc::ChainTip>, PortError<zaino_source::GetChainTipsError>> {
        if let Some(failure) = self.forced_failure() {
            return Err(failure);
        }
        let height = self.active_height();
        let Some(index) = self.valid_height(height) else {
            return Ok(Vec::new());
        };
        // One active tip and no side branches, matching a validator that has
        // never seen a fork.
        Ok(vec![domain::rpc::ChainTip {
            height: domain::Height::try_from(height).map_err(|e| port_fault(e.to_string()))?,
            hash: domain::BlockHash::from(self.blocks[index].hash().0),
            branch_len: 0,
            status: domain::rpc::ChainTipStatus::Active,
        }])
    }
}

impl zaino_source::GetAddressBalance for MockchainSource {
    async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<domain::AddressBalance, PortError<zaino_source::GetAddressBalanceError>> {
        let valid = GetAddressBalanceRequest::new(addresses)
            .valid_addresses()
            .map_err(|error| {
                port_fault::<zaino_source::GetAddressBalanceError>(format!(
                    "invalid address: {error}"
                ))
            })?;

        let network = mockchain_network();
        let matching = self.matching_transparent_outputs(&valid, &network);
        let spent = self.spent_transparent_outpoints();

        let mut balance = 0_u64;
        let mut received = 0_u64;
        for (outpoint, output) in matching {
            let value = u64::from(output.output.value());
            received = received.checked_add(value).ok_or_else(|| {
                port_fault::<zaino_source::GetAddressBalanceError>(
                    "address received amount overflowed u64",
                )
            })?;
            if !spent.contains(&outpoint) {
                balance = balance.checked_add(value).ok_or_else(|| {
                    port_fault::<zaino_source::GetAddressBalanceError>(
                        "address balance amount overflowed u64",
                    )
                })?;
            }
        }

        Ok(domain::AddressBalance {
            balance: domain::Zatoshis::new(balance)
                .map_err(|e| port_fault::<zaino_source::GetAddressBalanceError>(e.to_string()))?,
            received: domain::Zatoshis::new(received)
                .map_err(|e| port_fault::<zaino_source::GetAddressBalanceError>(e.to_string()))?,
        })
    }
}

impl zaino_source::GetAddressTxids for MockchainSource {
    async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: domain::Height,
        end: domain::Height,
    ) -> Result<Vec<domain::TransactionId>, PortError<zaino_source::GetAddressTxidsError>> {
        let valid = GetAddressBalanceRequest::new(addresses)
            .valid_addresses()
            .map_err(|error| {
                port_fault::<zaino_source::GetAddressTxidsError>(format!(
                    "invalid address: {error}"
                ))
            })?;

        let tip = self.active_height();
        if start > end {
            return Err(PortError::Domain(
                zaino_source::GetAddressTxidsError::InvalidRange { start, end },
            ));
        }
        if u32::from(start) > tip || u32::from(end) > tip {
            return Err(PortError::Domain(
                zaino_source::GetAddressTxidsError::InvalidRange { start, end },
            ));
        }

        let network = mockchain_network();
        let requested = normalize_requested_addresses_for_network(&valid, &network);
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        let matching = self.matching_transparent_outputs(&valid, &network);

        let mut hashes = Vec::new();
        for block_index in u32::from(start) as usize..=u32::from(end) as usize {
            for transaction in &self.blocks[block_index].transactions {
                if self.transaction_touches_addresses(transaction, &requested, &matching, &network)
                {
                    hashes.push(domain::TransactionId::from(transaction.hash().0));
                }
            }
        }
        Ok(hashes)
    }
}

impl zaino_source::GetAddressUtxos for MockchainSource {
    async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<domain::Utxo>, PortError<zaino_source::GetAddressUtxosError>> {
        let valid = GetAddressBalanceRequest::new(addresses)
            .valid_addresses()
            .map_err(|error| {
                port_fault::<zaino_source::GetAddressUtxosError>(format!(
                    "invalid address: {error}"
                ))
            })?;

        let network = mockchain_network();
        let spent = self.spent_transparent_outpoints();
        let mut unspent = self
            .matching_transparent_outputs(&valid, &network)
            .into_iter()
            .filter(|(outpoint, _)| !spent.contains(outpoint))
            .collect::<Vec<_>>();

        unspent.sort_by_key(|(_, output)| {
            (output.height, output.transaction_index, output.output_index)
        });

        unspent
            .into_iter()
            .map(|(_, output)| {
                Ok(domain::Utxo {
                    address: domain::TransparentAddress::new(output.address.to_string()),
                    txid: domain::TransactionId::from(output.transaction_hash.0),
                    output_index: output.output_index,
                    script: domain::Script::new(output.output.lock_script.as_raw_bytes().to_vec()),
                    satoshis: domain::Zatoshis::new(u64::from(output.output.value()))
                        .map_err(|e| port_fault(e.to_string()))?,
                    height: domain::Height::try_from(output.height.0)
                        .map_err(|e| port_fault(e.to_string()))?,
                })
            })
            .collect()
    }
}

/// The equihash solution's own bytes, without the length prefix its
/// serialization carries.
///
/// `Solution` keeps its bytes private and the domain header wants them raw, so
/// they are recovered by serializing and stepping over the leading compactsize.
fn equihash_solution_bytes(
    solution: &zebra_chain::work::equihash::Solution,
) -> Result<Vec<u8>, String> {
    let encoded = solution
        .zcash_serialize_to_vec()
        .map_err(|error| format!("equihash solution did not serialize: {error}"))?;

    let prefix = match encoded.first() {
        Some(&n) if n < 0xfd => 1,
        Some(0xfd) => 3,
        Some(0xfe) => 5,
        Some(0xff) => 9,
        _ => return Err("equihash solution serialized to nothing".to_string()),
    };
    if encoded.len() < prefix {
        return Err("equihash solution shorter than its length prefix".to_string());
    }
    Ok(encoded[prefix..].to_vec())
}

impl MockchainSource {
    /// Median time over the 11-block window ending at `index`, which is what
    /// `getblockdeltas` reports as `mediantime`.
    fn median_time_at(&self, index: usize) -> i64 {
        const WINDOW: usize = 11;
        let start = index.saturating_sub(WINDOW - 1);
        let mut times: Vec<i64> = (start..=index)
            .map(|i| self.blocks[i].header.time.timestamp())
            .collect();
        times.sort_unstable();
        times[times.len() / 2]
    }

    /// The address and value an outpoint paid, resolved through the txid index.
    ///
    /// A spend names the output it consumes rather than the address it debits,
    /// so the previous transaction has to be read back to attribute it.
    fn resolved_outpoint(
        &self,
        outpoint: &OutPoint,
        network: &zebra_chain::parameters::Network,
    ) -> Option<(domain::TransparentAddress, u64)> {
        let (_, prev) = self.txid_index.get(&outpoint.hash)?;
        let output = prev.outputs().get(outpoint.index as usize)?;
        let address = output.address(network)?;
        Some((
            domain::TransparentAddress::new(address.to_string()),
            u64::from(output.value()),
        ))
    }
}

impl zaino_source::GetBlockDeltas for MockchainSource {
    async fn get_block_deltas(
        &self,
        hash: domain::BlockHash,
    ) -> Result<domain::rpc::BlockDeltas, PortError<zaino_source::GetBlockDeltasError>> {
        let index = self
            .served_index_at_hash(hash)
            .ok_or_else(|| port_fault::<zaino_source::GetBlockDeltasError>("block not found"))?;
        let block = &self.blocks[index];
        let header = &block.header;
        let network = mockchain_network();
        let height = block.coinbase_height().ok_or_else(|| {
            port_fault::<zaino_source::GetBlockDeltasError>("getblockdeltas: block height missing")
        })?;

        let mut deltas = Vec::new();
        for (tx_index, transaction) in block.transactions.iter().enumerate() {
            let mut inputs = Vec::new();
            for (input_index, input) in transaction.inputs().iter().enumerate() {
                // Coinbase inputs spend nothing, so they debit no address.
                let Some(outpoint) = input.outpoint() else {
                    continue;
                };
                let Some((address, value)) = self.resolved_outpoint(&outpoint, &network) else {
                    continue;
                };
                inputs.push(domain::rpc::InputDelta {
                    address,
                    // Inputs are debits, so the amount leaves the address.
                    satoshis: domain::SignedZatoshis::new(-(value as i64)),
                    index: input_index as u32,
                    prev_txid: domain::TransactionId::from(outpoint.hash.0),
                    prev_output: outpoint.index,
                });
            }

            let mut outputs = Vec::new();
            for (output_index, output) in transaction.outputs().iter().enumerate() {
                // An output with no single derivable address credits nobody.
                let Some(address) = output.address(&network) else {
                    continue;
                };
                outputs.push(domain::rpc::OutputDelta {
                    address: domain::TransparentAddress::new(address.to_string()),
                    satoshis: domain::Zatoshis::new(u64::from(output.value()))
                        .map_err(|e| port_fault(e.to_string()))?,
                    index: output_index as u32,
                });
            }

            deltas.push(domain::rpc::BlockDelta {
                txid: domain::TransactionId::from(transaction.hash().0),
                index: tx_index as u32,
                inputs,
                outputs,
            });
        }

        let size = self
            .serialized_block_at(index)
            .map_err(port_fault::<zaino_source::GetBlockDeltasError>)?
            .len() as u64;

        Ok(domain::rpc::BlockDeltas {
            hash,
            confirmations: confirmations_from_depth(self.active_height().checked_sub(height.0)),
            size,
            height: domain::Height::try_from(height.0).map_err(|e| port_fault(e.to_string()))?,
            version: header.version,
            merkle_root: domain::MerkleRoot::from(header.merkle_root.0),
            deltas,
            time: header.time.timestamp() as u32,
            median_time: self.median_time_at(index) as u32,
            nonce: *header.nonce,
            bits: u32::from_be_bytes(header.difficulty_threshold.bytes_in_display_order()),
            difficulty: header.difficulty_threshold.relative_to_network(&network),
            previous_block_hash: Some(domain::BlockHash::from(header.previous_block_hash.0)),
            next_block_hash: self
                .next_block_hash(index)
                .map(|hash| domain::BlockHash::from(hash.0)),
        })
    }
}

impl zaino_source::GetAddressDeltas for MockchainSource {
    async fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: domain::Height,
        end: domain::Height,
    ) -> Result<Vec<domain::AddressDelta>, PortError<zaino_source::GetAddressDeltasError>> {
        use zaino_source::GetAddressTxids as _;

        let valid = GetAddressBalanceRequest::new(addresses.clone())
            .valid_addresses()
            .map_err(|error| {
                port_fault::<zaino_source::GetAddressDeltasError>(format!(
                    "invalid address: {error}"
                ))
            })?;
        let network = mockchain_network();
        let requested: Vec<String> = normalize_requested_addresses_for_network(&valid, &network)
            .into_iter()
            .map(|address| address.to_string())
            .collect();

        let txids = self
            .get_address_txids(addresses, start, end)
            .await
            .map_err(|error| port_fault(error.to_string()))?;

        // Receives only, matching every other implementation of this port: a
        // spend names an outpoint rather than an address, and attributing it
        // needs the spent transaction rather than this one.
        let mut deltas: Vec<domain::AddressDelta> = Vec::new();
        for txid in txids {
            let zebra_txid = zebra_chain::transaction::Hash(<[u8; 32]>::from(txid));
            let Some((block_index, transaction)) = self.txid_index.get(&zebra_txid) else {
                continue;
            };
            let height = self.block_height_at_index(*block_index);

            for (output_index, output) in transaction.outputs().iter().enumerate() {
                let Some(address) = output.address(&network) else {
                    continue;
                };
                let address = address.to_string();
                if !requested.iter().any(|wanted| wanted == &address) {
                    continue;
                }
                deltas.push(domain::AddressDelta {
                    satoshis: domain::SignedZatoshis::new(i64::from(output.value())),
                    txid,
                    index: output_index as u32,
                    height: domain::Height::try_from(height.0)
                        .map_err(|e| port_fault(e.to_string()))?,
                    address: domain::TransparentAddress::new(address),
                    block_index: Some(*block_index as u32),
                });
            }
        }

        deltas.sort_by_key(|delta| {
            (
                u32::from(delta.height),
                delta.block_index.unwrap_or(u32::MAX),
                delta.index,
            )
        });
        Ok(deltas)
    }
}

// ***** Questions the test vectors cannot answer *****
//
// Each panics with the same message its `BlockchainSource` counterpart carried:
// the vectors would have to be extended to serve these, and the panic names
// what is missing rather than inventing a plausible value.

impl zaino_source::GetTreestate for MockchainSource {
    async fn get_treestate(
        &self,
        _height: domain::Height,
    ) -> Result<domain::Treestate, PortError<zaino_source::GetTreestateError>> {
        // The `z_get_treestate` local path serves the mock by hash; the
        // node-passthrough fallback is never reached.
        unimplemented!("MockchainSource cannot serve the get_treestate_by_id passthrough")
    }
}

impl zaino_source::GetBlockchainInfo for MockchainSource {
    async fn get_blockchain_info(
        &self,
    ) -> Result<domain::BlockchainInfo, PortError<zaino_source::GetBlockchainInfoError>> {
        unimplemented!(
            "MockchainSource cannot serve get_blockchain_info until test vectors are extended"
        )
    }
}

impl zaino_source::GetNodeInfo for MockchainSource {
    async fn get_node_info(
        &self,
    ) -> Result<domain::rpc::NodeInfo, PortError<zaino_source::GetNodeInfoError>> {
        unimplemented!("MockchainSource cannot serve get_info until test vectors are extended")
    }
}

impl zaino_source::GetPeerInfo for MockchainSource {
    async fn get_peer_info(
        &self,
    ) -> Result<Vec<domain::rpc::PeerInfo>, PortError<zaino_source::GetPeerInfoError>> {
        unimplemented!("MockchainSource cannot serve get_peer_info until test vectors are extended")
    }
}

impl zaino_source::GetMiningInfo for MockchainSource {
    async fn get_mining_info(
        &self,
    ) -> Result<domain::rpc::MiningInfo, PortError<zaino_source::GetMiningInfoError>> {
        unimplemented!(
            "MockchainSource cannot serve get_mining_info until test vectors are extended"
        )
    }
}

impl zaino_source::GetBlockSubsidy for MockchainSource {
    async fn get_block_subsidy(
        &self,
        _height: domain::Height,
    ) -> Result<domain::rpc::BlockSubsidy, PortError<zaino_source::GetBlockSubsidyError>> {
        unimplemented!(
            "MockchainSource cannot serve get_block_subsidy until test vectors are extended"
        )
    }
}

impl zaino_source::GetNetworkSolPs for MockchainSource {
    async fn get_network_sol_ps(
        &self,
        _blocks: Option<u32>,
        _height: Option<domain::Height>,
    ) -> Result<u64, PortError<zaino_source::GetNetworkSolPsError>> {
        unimplemented!(
            "MockchainSource cannot serve get_network_sol_ps until test vectors are extended"
        )
    }
}

impl zaino_source::SendRawTransaction for MockchainSource {
    async fn send_raw_transaction(
        &self,
        _transaction: Vec<u8>,
    ) -> Result<domain::TransactionId, PortError<zaino_source::SendRawTransactionError>> {
        // The mock chain has no mempool to accept submissions.
        unimplemented!("MockchainSource cannot serve send_raw_transaction")
    }
}

impl zaino_source::GetSpentInfo for MockchainSource {
    async fn get_spent_info(
        &self,
        _outpoint: domain::rpc::SpentOutpoint,
    ) -> Result<domain::rpc::SpentInfo, PortError<zaino_source::GetSpentInfoError>> {
        unimplemented!(
            "MockchainSource cannot serve get_spent_info until test vectors are extended"
        )
    }
}

impl zaino_source::GetTxOut for MockchainSource {
    async fn get_tx_out(
        &self,
        _txid: domain::TransactionId,
        _index: domain::OutputIndex,
        _include_mempool: bool,
    ) -> Result<Option<domain::rpc::TxOut>, PortError<zaino_source::GetTxOutError>> {
        unimplemented!("MockchainSource cannot serve get_tx_out until test vectors are extended")
    }
}

#[cfg(test)]
mod mine_blocks {
    use crate::chain_index::source::BlockchainSource;
    use crate::chain_index::tests::vectors::{build_active_mockchain_source, load_test_vectors};

    /// `mine_blocks` must fire the `blocks_received_broadcaster`;
    /// `mine_blocks_silent` must not. The two methods are *defined* by
    /// that distinction — `mine_blocks_silent` exists solely to advance
    /// the active height without waking subscribers, and skew tests
    /// rely on that.
    ///
    /// Pins the contract at the source so any future drift of the
    /// shape (field removed, override removed, `send_replace` call
    /// dropped from `mine_blocks`) fails here instead of leaking into
    /// higher-level tests.
    #[test]
    fn mine_blocks_fires_broadcaster_silent_does_not() {
        let vectors = load_test_vectors().expect("test vectors load");
        // active_height = 0 leaves room for both mine calls to advance.
        let mockchain = build_active_mockchain_source(0, vectors.blocks);

        let mut rx = mockchain
            .subscribe_to_blocks_received()
            .expect("MockchainSource must override subscribe_to_blocks_received to return Some");

        // Fresh subscriber: the watch sender has been live since
        // construction but no `send_replace` has fired yet, so the
        // initial value is unseen. Mark it seen so subsequent
        // `has_changed()` calls reflect only post-arming activity.
        rx.mark_unchanged();
        assert!(
            !rx.has_changed().expect("watch sender alive"),
            "freshly-marked subscriber should see no pending change",
        );

        mockchain.source().mine_blocks(1);
        assert!(
            rx.has_changed().expect("watch sender alive"),
            "mine_blocks must fire blocks_received_broadcaster — \
             if this fails, the broadcaster wiring on MockchainSource has \
             regressed (missing field, missing send_replace, or missing \
             subscribe_to_blocks_received override)",
        );

        rx.mark_unchanged();

        mockchain.source().mine_blocks_silent(1);
        assert!(
            !rx.has_changed().expect("watch sender alive"),
            "mine_blocks_silent must NOT fire blocks_received_broadcaster \
             (the only behavioural difference from mine_blocks)",
        );
    }
}
