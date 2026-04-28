//! Mock BlockchainSourceResult implementation.

use super::*;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use zaino_common::network::ActivationHeights;
use zaino_fetch::jsonrpsee::response::address_deltas::BlockInfo;
use zebra_chain::{block::Block, orchard::tree as orchard, sapling::tree as sapling};
use zebra_chain::{
    block::Height,
    parameters::NetworkKind,
    serialization::BytesInDisplayOrder as _,
    transparent::{Address, OutPoint, Output, OutputIndex},
};
use zebra_rpc::{client::TransactionObject, methods::ValidateAddresses as _};
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
    zaino_common::Network::Regtest(ActivationHeights::default()).to_zebra_network()
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
}

impl MockchainSource {
    /// Creates a new MockchainSource.
    /// All inputs must be the same length, and ordered by ascending height starting from 0.
    #[allow(clippy::type_complexity)]
    pub(crate) fn new(
        blocks: Vec<Arc<Block>>,
        roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
        treestates: Vec<(Vec<u8>, Vec<u8>)>,
        hashes: Vec<BlockHash>,
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
        let tip_height = blocks.len().saturating_sub(1) as u32;
        let txid_index = build_txid_index(&blocks);
        Self {
            blocks,
            roots,
            treestates,
            hashes,
            txid_index,
            active_chain_height: Arc::new(AtomicU32::new(tip_height)),
            force_requests_against_source_to_fail: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
        }
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
        }
    }

    /// When set to true, `get_best_block_height` and `get_best_block_hash`
    /// return `BlockchainSourceError::Unrecoverable`.
    pub(crate) fn set_failing(&self, fail: bool) {
        self.force_requests_against_source_to_fail
            .store(fail, Ordering::SeqCst);
    }

    pub(crate) fn mine_blocks(&self, blocks: u32) {
        // len() returns one-indexed length, height is zero-indexed.
        let max_height = self.max_chain_height();
        let _ =
            self.active_chain_height
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    let target = current.saturating_add(blocks).min(max_height);
                    if target == current {
                        None
                    } else {
                        Some(target)
                    }
                });
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

#[async_trait]
impl BlockchainSource for MockchainSource {
    // ********** Block methods **********

    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match id {
            HashOrHeight::Height(h) => {
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
    async fn get_treestate(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        let active_chain_height = self.active_height() as usize; // serve up to active tip

        if let Some(height) = self.hashes.iter().position(|h| h == &id) {
            if height <= active_chain_height {
                let (sapling_state, orchard_state) = &self.treestates[height];
                Ok((Some(sapling_state.clone()), Some(orchard_state.clone())))
            } else {
                Ok((None, None))
            }
        } else {
            Ok((None, None))
        }
    }

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
        }

        Ok(subtree_roots)
    }

    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        let active_chain_height = self.active_height() as usize; // serve up to active tip

        if let Some(height) = self.hashes.iter().position(|h| h == &id) {
            if height <= active_chain_height {
                Ok(self.roots[height])
            } else {
                Ok((None, None))
            }
        } else {
            Ok((None, None))
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
}
