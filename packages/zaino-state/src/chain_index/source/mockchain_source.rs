//! Mock BlockchainSourceResult implementation.

use super::validator_connector::{
    assemble_block_deltas, build_block_header_object, build_verbose_block,
    confirmations_from_depth, final_orchard_root, final_sapling_root, median_of_block_times,
    zebra_block_header_to_wire,
};
use super::*;
use std::collections::{HashMap, HashSet};
use std::str::FromStr as _;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};
use zaino_common::network::ActivationHeights;
use zaino_fetch::jsonrpsee::response::{
    address_deltas::BlockInfo, block_deltas::BlockDeltas, block_header::GetBlockHeader,
};
use zebra_chain::{block::Block, orchard::tree as orchard, sapling::tree as sapling};
use zebra_chain::{
    block::{Height, SerializedBlock},
    parameters::NetworkKind,
    serialization::{BytesInDisplayOrder as _, ZcashSerialize as _},
    transparent::{Address, OutPoint, Output, OutputIndex},
};
use zebra_rpc::{
    client::{BlockObject, HexData, Input, TransactionObject},
    methods::{GetBlock, GetBlockHeaderResponse, GetBlockTransaction, ValidateAddresses as _},
};
use zebra_state::HashOrHeight;

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
fn mockchain_network() -> zebra_chain::parameters::Network {
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

    /// Resolves a hash-or-height request to a block index within the active chain.
    fn resolve_index(&self, id: &HashOrHeight) -> Option<usize> {
        match id {
            HashOrHeight::Height(height) => self.valid_height(height.0),
            HashOrHeight::Hash(hash) => self.valid_hash(hash),
        }
    }

    /// The zebra hash of the block after `height_index`, if it is within the active chain.
    fn next_block_hash(&self, height_index: usize) -> Option<zebra_chain::block::Hash> {
        let next = height_index + 1;
        (next <= self.active_chain_height_as_usize()).then(|| self.blocks[next].hash())
    }

    /// Builds the zebra `getblockheader` response for the block at `height_index` from the
    /// test-vector block and tree-root data, using the same builders as the validator path.
    fn block_header_response_at(
        &self,
        height_index: usize,
        verbose: bool,
    ) -> BlockchainSourceResult<GetBlockHeaderResponse> {
        let block = &self.blocks[height_index];
        let header = &block.header;
        if !verbose {
            return Ok(GetBlockHeaderResponse::Raw(HexData(
                header
                    .zcash_serialize_to_vec()
                    .map_err(BlockchainSourceError::unrecoverable)?,
            )));
        }

        let network = mockchain_network();
        let hash = block.hash();
        let height = block.coinbase_height().ok_or_else(|| {
            BlockchainSourceError::Unrecoverable("block missing coinbase height".to_string())
        })?;
        let depth = self.active_height().checked_sub(height.0);
        let confirmations = confirmations_from_depth(depth);

        let (sapling_root_bytes, sapling_tree_size) = match &self.roots[height_index].0 {
            Some((root, size)) => (<[u8; 32]>::from(*root), *size),
            None => ([0u8; 32], 0),
        };
        let final_sapling_root = final_sapling_root(sapling_root_bytes, height, &network);
        let next_block_hash = self.next_block_hash(height_index);

        let header_obj = build_block_header_object(
            header,
            hash,
            height,
            confirmations,
            final_sapling_root,
            sapling_tree_size,
            next_block_hash,
            &network,
        )?;
        Ok(GetBlockHeaderResponse::Object(Box::new(header_obj)))
    }

    /// Median time past over the 11-block window ending at `start`, walking backwards via
    /// verbosity-1 `getblock` lookups against the mock vectors.
    async fn median_time_past(&self, start: &BlockObject) -> BlockchainSourceResult<i64> {
        const MEDIAN_TIME_PAST_WINDOW: usize = 11;
        let mut times = Vec::with_capacity(MEDIAN_TIME_PAST_WINDOW);
        let start_time = start.time().ok_or_else(|| {
            BlockchainSourceError::Unrecoverable("getblockdeltas: start block missing time".into())
        })?;
        times.push(start_time);

        let mut prev = start.previous_block_hash();
        for _ in 0..(MEDIAN_TIME_PAST_WINDOW - 1) {
            let Some(hash) = prev else {
                break; // genesis
            };
            match self
                .get_block_verbose(HashOrHeight::Hash(hash), Some(1))
                .await
            {
                Ok(GetBlock::Object(object)) => {
                    if let Some(time) = object.time() {
                        times.push(time);
                    }
                    prev = object.previous_block_hash();
                }
                Ok(GetBlock::Raw(_)) => break,
                Err(_) => break,
            }
        }

        median_of_block_times(times)
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

impl BlockchainSource for MockchainSource {
    // ********** Block methods **********

    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match id {
            HashOrHeight::Height(h) => {
                // One-shot test hook fires before the active-height check so
                // a hook that mutates active height (typically `mine_blocks`)
                // is visible to this same call's `valid_height` lookup.
                let hook = self
                    .get_block_hook
                    .lock()
                    .expect("get_block_hook mutex poisoned")
                    .take();
                if let Some(f) = hook {
                    f();
                }
                let Some(height_index) = self.valid_height(h.0) else {
                    return Ok(None);
                };
                Ok(Some(Arc::clone(&self.blocks[height_index])))
            }
            HashOrHeight::Hash(hash) => {
                let Some(hash_index) = self.valid_hash(&hash) else {
                    return Ok(None);
                };

                Ok(Some(Arc::clone(&self.blocks[hash_index])))
            }
        }
    }

    async fn get_block_verbose(
        &self,
        hash_or_height: HashOrHeight,
        verbosity: Option<u8>,
    ) -> BlockchainSourceResult<GetBlock> {
        let verbosity = verbosity.unwrap_or(1);
        let height_index = self
            .resolve_index(&hash_or_height)
            .ok_or_else(|| BlockchainSourceError::Unrecoverable("block not found".to_string()))?;

        match verbosity {
            0 => Ok(GetBlock::Raw(SerializedBlock::from(Arc::clone(
                &self.blocks[height_index],
            )))),
            1 | 2 => {
                let block = &self.blocks[height_index];
                let network = mockchain_network();

                let GetBlockHeaderResponse::Object(header_obj) =
                    self.block_header_response_at(height_index, true)?
                else {
                    unreachable!("`true` yields an object")
                };

                let height = block.coinbase_height().ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable(
                        "block missing coinbase height".to_string(),
                    )
                })?;
                let (orchard_root_bytes, orchard_tree_size) = match &self.roots[height_index].1 {
                    Some((root, size)) => (<[u8; 32]>::from(*root), *size),
                    None => ([0u8; 32], 0),
                };
                let final_orchard_root = final_orchard_root(orchard_root_bytes, height, &network);

                // The verbose block reports the block's serialized byte size; the mock has the
                // full block, so serialize it to measure.
                let size = block
                    .zcash_serialize_to_vec()
                    .map_err(BlockchainSourceError::unrecoverable)?
                    .len() as i64;

                // `chain_supply` / `value_pools` are cumulative pool balances that the test
                // vectors do not carry, so they are `None` for the mock.
                Ok(build_verbose_block(
                    &header_obj,
                    block,
                    verbosity,
                    size,
                    final_orchard_root,
                    orchard_tree_size,
                    // The mock's test vectors do not carry an ironwood tree size.
                    0,
                    None,
                    None,
                    &network,
                ))
            }
            more_than_two => Err(BlockchainSourceError::Unrecoverable(format!(
                "invalid verbosity of {more_than_two}"
            ))),
        }
    }

    async fn get_block_header(
        &self,
        hash: String,
        verbose: bool,
    ) -> BlockchainSourceResult<GetBlockHeader> {
        let hash_or_height =
            HashOrHeight::from_str(&hash).map_err(BlockchainSourceError::unrecoverable)?;
        let height_index = self.resolve_index(&hash_or_height).ok_or_else(|| {
            BlockchainSourceError::Unrecoverable("block height not in best chain".to_string())
        })?;
        let header = self.block_header_response_at(height_index, verbose)?;
        zebra_block_header_to_wire(header)
    }

    async fn get_block_deltas(&self, hash: String) -> BlockchainSourceResult<BlockDeltas> {
        let hash_or_height =
            HashOrHeight::from_str(&hash).map_err(BlockchainSourceError::unrecoverable)?;
        let GetBlock::Object(object) = self.get_block_verbose(hash_or_height, Some(2)).await?
        else {
            return Err(BlockchainSourceError::Unrecoverable(
                "getblockdeltas: unexpected raw block".to_string(),
            ));
        };

        // The mock holds every transaction, so prevout resolution is a direct index lookup.
        let mut prevtx_cache: HashMap<
            zebra_chain::transaction::Hash,
            Arc<zebra_chain::transaction::Transaction>,
        > = HashMap::new();
        for tx in object.tx() {
            let GetBlockTransaction::Object(txo) = tx else {
                continue;
            };
            for input in txo.inputs() {
                let Input::NonCoinbase { txid: prevtxid, .. } = input else {
                    continue;
                };
                let prev_hash = zebra_chain::transaction::Hash::from_str(prevtxid)
                    .map_err(BlockchainSourceError::unrecoverable)?;
                if prevtx_cache.contains_key(&prev_hash) {
                    continue;
                }
                let (_height, prev_tx) = self.txid_index.get(&prev_hash).ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable(format!(
                        "getblockdeltas: prevout tx {prevtxid} not found in mock chain"
                    ))
                })?;
                prevtx_cache.insert(prev_hash, Arc::clone(prev_tx));
            }
        }

        let median_time = self.median_time_past(&object).await?;
        assemble_block_deltas(&object, &prevtx_cache, median_time, &mockchain_network())
    }

    // ********** Chain methods **********

    async fn get_difficulty(&self) -> BlockchainSourceResult<f64> {
        let tip_index = self.active_chain_height_as_usize();
        let tip_block = self.blocks.get(tip_index).ok_or_else(|| {
            BlockchainSourceError::Unrecoverable("mock chain has no tip block".to_string())
        })?;
        Ok(tip_block
            .header
            .difficulty_threshold
            .relative_to_network(&mockchain_network()))
    }

    async fn get_blockchain_info(
        &self,
    ) -> BlockchainSourceResult<zebra_rpc::methods::GetBlockchainInfoResponse> {
        // Needs cumulative pool value balances (TipPoolValues) and on-disk size, which the
        // vectors don't carry. Test vectors must be extended to serve this method; tracked
        // by the update-test-vectors follow-up (see "Future work").
        unimplemented!(
            "MockchainSource cannot serve get_blockchain_info until test vectors are extended"
        )
    }

    // ********** Node-passthrough methods **********
    //
    // These are node-only RPCs with no chain data in the vectors. Test vectors must be
    // extended to let MockchainSource serve them; tracked by the update-test-vectors
    // follow-up (see "Future work").

    async fn get_info(&self) -> BlockchainSourceResult<zebra_rpc::methods::GetInfo> {
        unimplemented!("MockchainSource cannot serve get_info until test vectors are extended")
    }

    async fn get_peer_info(
        &self,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::peer_info::GetPeerInfo> {
        unimplemented!("MockchainSource cannot serve get_peer_info until test vectors are extended")
    }

    /// Records the release so teardown tests can assert the index shuts its
    /// source down (the mock owns no real background work).
    fn shutdown(&self) {
        self.shutdown_called.store(true, Ordering::SeqCst);
    }

    /// A single active tip at the mockchain's current active height, matching
    /// what a validator with no side branches reports.
    async fn get_chain_tips(
        &self,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::chain_tips::GetChainTipsResponse>
    {
        if self
            .force_requests_against_source_to_fail
            .load(Ordering::SeqCst)
        {
            return Err(BlockchainSourceError::Unrecoverable(
                "forced source failure".into(),
            ));
        }
        let height = self.active_height();
        let Some(index) = self.valid_height(height) else {
            return Ok(vec![]);
        };
        Ok(vec![
            zaino_fetch::jsonrpsee::response::chain_tips::ChainTip::new(
                height,
                self.blocks[index].hash().to_string(),
                0,
                zaino_fetch::jsonrpsee::response::chain_tips::ChainTipStatus::Active,
            ),
        ])
    }

    async fn get_block_subsidy(
        &self,
        _height: u32,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::block_subsidy::GetBlockSubsidy>
    {
        unimplemented!(
            "MockchainSource cannot serve get_block_subsidy until test vectors are extended"
        )
    }

    async fn get_mining_info(
        &self,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::mining_info::GetMiningInfoWire>
    {
        unimplemented!(
            "MockchainSource cannot serve get_mining_info until test vectors are extended"
        )
    }

    async fn get_tx_out(
        &self,
        _txid: String,
        _n: u32,
        _include_mempool: Option<bool>,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::GetTxOutResponse> {
        unimplemented!("MockchainSource cannot serve get_tx_out until test vectors are extended")
    }

    async fn get_spent_info(
        &self,
        _request: zaino_fetch::jsonrpsee::response::GetSpentInfoRequest,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::GetSpentInfoResponse> {
        unimplemented!(
            "MockchainSource cannot serve get_spent_info until test vectors are extended"
        )
    }

    async fn get_network_sol_ps(
        &self,
        _blocks: Option<i32>,
        _height: Option<i32>,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::GetNetworkSolPsResponse> {
        unimplemented!(
            "MockchainSource cannot serve get_network_sol_ps until test vectors are extended"
        )
    }

    async fn send_raw_transaction(
        &self,
        _raw_transaction_hex: String,
    ) -> BlockchainSourceResult<zebra_rpc::methods::SentTransactionHash> {
        // The mock chain has no mempool to accept submissions.
        unimplemented!("MockchainSource cannot serve send_raw_transaction")
    }

    async fn get_treestate_by_id(
        &self,
        _hash_or_height: String,
    ) -> BlockchainSourceResult<zebra_rpc::client::GetTreestateResponse> {
        // The `z_get_treestate` local path serves the mock; the node-passthrough fallback
        // is never reached, so this is left unimplemented.
        unimplemented!("MockchainSource cannot serve the get_treestate_by_id passthrough")
    }

    // ********** Transaction methods **********

    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    > {
        let zebra_txid = zebra_chain::transaction::Hash::from(txid.0);
        let active_chain_height = self.active_height() as usize;
        let mempool_height = active_chain_height + 1;

        let Some((stored_height, tx)) = self.txid_index.get(&zebra_txid) else {
            return Ok(None);
        };

        if *stored_height <= active_chain_height {
            return Ok(Some((
                Arc::clone(tx),
                GetTransactionLocation::BestChain(zebra_chain::block::Height(
                    *stored_height as u32,
                )),
            )));
        }
        if *stored_height == mempool_height {
            return Ok(Some((Arc::clone(tx), GetTransactionLocation::Mempool)));
        }
        Ok(None)
    }

    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        let mempool_height = self.active_height() as usize + 1;

        let txids = if mempool_height < self.blocks.len() {
            self.blocks[mempool_height]
                .transactions
                .iter()
                .filter(|tx| !tx.is_coinbase()) // <-- exclude coinbase
                .map(|tx| tx.hash())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        Ok(Some(txids))
    }

    // ********** Chain methods **********

    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        if self
            .force_requests_against_source_to_fail
            .load(Ordering::SeqCst)
        {
            return Err(BlockchainSourceError::Unrecoverable(
                "forced source failure".into(),
            ));
        }
        let active_chain_height = self.active_height() as usize;

        if self.blocks.is_empty() || active_chain_height > self.max_chain_height() as usize {
            return Ok(None);
        }

        Ok(Some(self.blocks[active_chain_height].hash()))
    }

    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>> {
        if self
            .force_requests_against_source_to_fail
            .load(Ordering::SeqCst)
        {
            return Err(BlockchainSourceError::Unrecoverable(
                "forced source failure".into(),
            ));
        }
        let active_chain_height = self.active_height() as usize;

        if self.blocks.is_empty() || active_chain_height > self.max_chain_height() as usize {
            return Ok(None);
        }

        let Some(height) = self.blocks[active_chain_height].coinbase_height() else {
            return Err(BlockchainSourceError::Unrecoverable(format!(
                "active chain block at index {active_chain_height} has no coinbase height"
            )));
        };

        Ok(Some(height))
    }

    /// Returns the sapling and orchard treestate by hash
    ///
    /// TODO: Update test vectors to support ironwood.
    async fn get_treestate(&self, id: BlockHash) -> BlockchainSourceResult<super::TreestateBytes> {
        let active_chain_height = self.active_height() as usize; // serve up to active tip

        if let Some(height) = self.hashes.iter().position(|h| h == &id) {
            if height <= active_chain_height {
                let (sapling_state, orchard_state) = &self.treestates[height];
                Ok((
                    Some(super::PoolTreestate {
                        final_root: None,
                        final_state: sapling_state.clone(),
                    }),
                    Some(super::PoolTreestate {
                        final_root: None,
                        final_state: orchard_state.clone(),
                    }),
                    None,
                ))
            } else {
                Ok((None, None, None))
            }
        } else {
            Ok((None, None, None))
        }
    }

    /// TODO: Update test vectors to support ironwood.
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>> {
        let requested_limit = max_entries.map(usize::from).unwrap_or(usize::MAX);

        if requested_limit == 0 {
            return Ok(Vec::new());
        }

        let mut subtree_roots: Vec<([u8; 32], u32)> = Vec::new();

        match pool {
            ShieldedPool::Sapling => {
                let mut note_commitment_tree = sapling::NoteCommitmentTree::default();

                for block_index in 0..=self.active_chain_height_as_usize() {
                    let block = &self.blocks[block_index];
                    let height = self.block_height_at_index(block_index);

                    for note_commitment in block.sapling_note_commitments() {
                        note_commitment_tree
                            .append(*note_commitment)
                            .map_err(|error| {
                                BlockchainSourceError::Unrecoverable(format!(
                                    "could not append Sapling note commitment to tree: {error}"
                                ))
                            })?;

                        let Some((subtree_index, subtree_root)) =
                            note_commitment_tree.completed_subtree_index_and_root()
                        else {
                            continue;
                        };

                        if subtree_index.0 < start_index {
                            continue;
                        }

                        subtree_roots.push((subtree_root.to_bytes(), height.0));

                        if subtree_roots.len() == requested_limit {
                            return Ok(subtree_roots);
                        }
                    }
                }
            }
            ShieldedPool::Orchard => {
                let mut note_commitment_tree = orchard::NoteCommitmentTree::default();

                for block_index in 0..=self.active_chain_height_as_usize() {
                    let block = &self.blocks[block_index];
                    let height = self.block_height_at_index(block_index);

                    for note_commitment in block.orchard_note_commitments() {
                        note_commitment_tree
                            .append(*note_commitment)
                            .map_err(|error| {
                                BlockchainSourceError::Unrecoverable(format!(
                                    "could not append Orchard note commitment to tree: {error}"
                                ))
                            })?;

                        let Some((subtree_index, subtree_root)) =
                            note_commitment_tree.completed_subtree_index_and_root()
                        else {
                            continue;
                        };

                        if subtree_index.0 < start_index {
                            continue;
                        }

                        subtree_roots.push((subtree_root.to_repr(), height.0));

                        if subtree_roots.len() == requested_limit {
                            return Ok(subtree_roots);
                        }
                    }
                }
            }
            ShieldedPool::Ironwood => {}
        }

        Ok(subtree_roots)
    }

    /// TODO: Update test vectors to support ironwood.
    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        let active_chain_height = self.active_height() as usize; // serve up to active tip

        if let Some(height) = self.hashes.iter().position(|h| h == &id) {
            if height <= active_chain_height {
                let (sapling, orchard) = self.roots[height];
                Ok((sapling, orchard, None))
            } else {
                Ok((None, None, None))
            }
        } else {
            Ok((None, None, None))
        }
    }

    // ********** Transparent address methods **********

    async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> BlockchainSourceResult<GetAddressDeltasResponse> {
        let (addresses, start_raw, end_raw, chain_info) = match &params {
            GetAddressDeltasParams::Filtered {
                addresses,
                start,
                end,
                chain_info,
            } => (addresses.clone(), *start, *end, *chain_info),
            GetAddressDeltasParams::Address(address) => (vec![address.clone()], 0, 0, false),
        };

        let valid_addresses = GetAddressBalanceRequest::new(addresses.clone())
            .valid_addresses()
            .map_err(|error| {
                BlockchainSourceError::Unrecoverable(format!("invalid address: {error}"))
            })?;

        let network = mockchain_network();

        let mut normalized_addresses =
            normalize_requested_addresses_for_network(&valid_addresses, &network)
                .into_iter()
                .map(|address| address.to_string())
                .collect::<Vec<_>>();

        normalized_addresses.sort();

        let tip = Height(self.active_height());

        let mut start = Height(start_raw);
        let mut end = Height(end_raw);

        if end == Height(0) || end > tip {
            end = tip;
        }

        if start > tip {
            start = tip;
        }

        let tx_ids_request =
            GetAddressTxIdsRequest::new(addresses.clone(), Some(start.0), Some(end.0));

        let txids = self.get_address_txids(tx_ids_request).await?;

        let mut transactions: Vec<Box<TransactionObject>> = Vec::with_capacity(txids.len());

        for txid in txids {
            let Some((transaction, location)) = self.get_transaction(txid).await? else {
                continue;
            };

            let height = match location {
                GetTransactionLocation::BestChain(height) => Some(height),
                GetTransactionLocation::NonbestChain | GetTransactionLocation::Mempool => None,
            };

            transactions.push(Box::new(TransactionObject::from_transaction(
                transaction.clone(),
                height,
                None,
                &network,
                None,
                None,
                Some(matches!(location, GetTransactionLocation::BestChain(_))),
                transaction.hash(),
            )));
        }

        let deltas = GetAddressDeltasResponse::process_transactions_to_deltas(
            &transactions,
            &normalized_addresses,
        );

        if chain_info {
            let Some(start_index) = self.valid_height(start.0) else {
                return Err(BlockchainSourceError::Unrecoverable(format!(
                    "Block not found at height {}",
                    start.0
                )));
            };

            let Some(end_index) = self.valid_height(end.0) else {
                return Err(BlockchainSourceError::Unrecoverable(format!(
                    "Block not found at height {}",
                    end.0
                )));
            };

            Ok(GetAddressDeltasResponse::WithChainInfo {
                deltas,
                start: BlockInfo::new(
                    hex::encode(self.blocks[start_index].hash().bytes_in_display_order()),
                    start.0,
                ),
                end: BlockInfo::new(
                    hex::encode(self.blocks[end_index].hash().bytes_in_display_order()),
                    end.0,
                ),
            })
        } else {
            Ok(GetAddressDeltasResponse::Simple(deltas))
        }
    }

    async fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<AddressBalance> {
        let valid_addresses = address_strings.valid_addresses().map_err(|error| {
            BlockchainSourceError::Unrecoverable(format!("invalid address: {error}"))
        })?;

        let network = mockchain_network();
        let matching_outputs = self.matching_transparent_outputs(&valid_addresses, &network);
        let spent_outpoints = self.spent_transparent_outpoints();

        let mut balance = 0_u64;
        let mut received = 0_u64;

        for (outpoint, matching_output) in matching_outputs {
            let value = u64::from(matching_output.output.value());

            received = received.checked_add(value).ok_or_else(|| {
                BlockchainSourceError::Unrecoverable(
                    "address received amount overflowed u64".to_string(),
                )
            })?;

            if !spent_outpoints.contains(&outpoint) {
                balance = balance.checked_add(value).ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable(
                        "address balance amount overflowed u64".to_string(),
                    )
                })?;
            }
        }

        Ok(AddressBalance::new(balance, received))
    }

    async fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> BlockchainSourceResult<Vec<TransactionHash>> {
        let (addresses, start, end) = request.into_parts();

        let valid_addresses = GetAddressBalanceRequest::new(addresses)
            .valid_addresses()
            .map_err(|error| {
                BlockchainSourceError::Unrecoverable(format!("invalid address: {error}"))
            })?;

        let chain_height = Height(self.active_height());

        if start > end {
            return Err(BlockchainSourceError::Unrecoverable(format!(
                "start {start:?} must be less than or equal to end {end:?}"
            )));
        }

        if Height(start) > chain_height || Height(end) > chain_height {
            return Err(BlockchainSourceError::Unrecoverable(format!(
            "start {start:?} and end {end:?} must both be less than or equal to the chain tip {chain_height:?}"
        )));
        }

        let network = mockchain_network();
        let requested_addresses =
            normalize_requested_addresses_for_network(&valid_addresses, &network);
        let matching_outputs = self.matching_transparent_outputs(&valid_addresses, &network);

        let mut transaction_hashes = Vec::new();

        if requested_addresses.is_empty() {
            return Ok(transaction_hashes);
        }

        for block_index in start as usize..=end as usize {
            let block = &self.blocks[block_index];

            for transaction in &block.transactions {
                if self.transaction_touches_addresses(
                    transaction,
                    &requested_addresses,
                    &matching_outputs,
                    &network,
                ) {
                    transaction_hashes.push(TransactionHash::from(transaction.hash()));
                }
            }
        }

        Ok(transaction_hashes)
    }

    async fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<Vec<GetAddressUtxos>> {
        let valid_addresses = address_strings.valid_addresses().map_err(|error| {
            BlockchainSourceError::Unrecoverable(format!("invalid address: {error}"))
        })?;

        let network = mockchain_network();
        let matching_outputs = self.matching_transparent_outputs(&valid_addresses, &network);
        let spent_outpoints = self.spent_transparent_outpoints();

        let mut unspent_outputs = matching_outputs
            .into_iter()
            .filter(|(outpoint, _matching_output)| !spent_outpoints.contains(outpoint))
            .collect::<Vec<_>>();

        unspent_outputs.sort_by_key(|(_outpoint, matching_output)| {
            (
                matching_output.height,
                matching_output.transaction_index,
                matching_output.output_index,
            )
        });

        let utxos = unspent_outputs
            .into_iter()
            .map(|(_outpoint, matching_output)| {
                GetAddressUtxos::new(
                    matching_output.address,
                    matching_output.transaction_hash,
                    OutputIndex::from_index(matching_output.output_index),
                    matching_output.output.lock_script.clone(),
                    u64::from(matching_output.output.value()),
                    matching_output.height,
                )
            })
            .collect();

        Ok(utxos)
    }

    // ********** Utility methods **********

    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        Ok(None)
    }

    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.blocks_received_broadcaster.subscribe())
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

        mockchain.mine_blocks(1);
        assert!(
            rx.has_changed().expect("watch sender alive"),
            "mine_blocks must fire blocks_received_broadcaster — \
             if this fails, the broadcaster wiring on MockchainSource has \
             regressed (missing field, missing send_replace, or missing \
             subscribe_to_blocks_received override)",
        );

        rx.mark_unchanged();

        mockchain.mine_blocks_silent(1);
        assert!(
            !rx.has_changed().expect("watch sender alive"),
            "mine_blocks_silent must NOT fire blocks_received_broadcaster \
             (the only behavioural difference from mine_blocks)",
        );
    }
}
