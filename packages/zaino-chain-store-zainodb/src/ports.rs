//! This backend's implementation of the `zaino-chain-store` ports.
//!
//! The seam between what is on disk and what a consumer sees. Everything below
//! this module speaks the store's own vocabulary — `IndexedBlock`, `TxLocation`,
//! `TxOutCompact`, `Capability` — and nothing above it does. The conversions
//! live here rather than on the persisted types because a persisted type must
//! do nothing but cross the serde boundary (see the project's
//! persistence-boundary rule); knowing what a domain consumer wants is not that.
//!
//! # What this module does *not* do
//!
//! It does not re-implement any read. Every method delegates to the existing
//! reader and converts the answer. Where a domain method has no single backend
//! counterpart — `unspent_output`, which is an existence check and a spend check
//! — the composition is here, because it is a question about the domain rather
//! than about LMDB.

use core::future::Future;

use zaino_chain_store::{
    ChainStoreError, ChainStoreFreezeSink, ChainStoreIngest, ChainStoreReader, ChainStoreService,
    ChainStoreSource, ChainStoreSourceError, CompactBlockRead, MigrationState, PoolFilter,
    SchemaVersion, SpenderRef, SpentOutputIndex, StoreCapabilities, StoreCapability, StoreSchema,
    StoreWatermark, StoredAddress, StoredBlock, StoredBlockRead, StoredTx, StoredTxOut,
    TransactionIndex, TxOutSetAccumulator, TxOutSetIndex,
};
use zaino_primitives::types::{
    BlockHash as DomainBlockHash, BlockHeader, BlockRef, BlockTxPosition,
    ChainWork as DomainChainWork, CompactBlock, EncryptedCiphertext, Height as DomainHeight,
    Nullifier, OrchardAction, Outpoint as DomainOutpoint, PreIndexCompactTx, SaplingOutput, Script,
    ScriptType, SignedZatoshis, TransactionId, TransparentInput, TransparentOutput, TreeRootInfo,
    TreeRoots, TxIndex, Zatoshis,
};
use zaino_status::StatusType;

use crate::error::StoreError;
use crate::store::capability::{Capability, CapabilityRequest, DbMetadata, MigrationStatus};
use crate::store::finalised_source::v1::DB_VERSION_V1;
use crate::store::reader::DbReader;
use crate::store::FinalisedState;
use crate::types::{
    BlockHash, CommitmentTreeData, CompactTxData, Height, IndexedBlock, Outpoint, TransactionHash,
    TransparentCompactTx, TxLocation, TxOutCompact,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// This backend's error, as the domain names it.
///
/// The mapping is deliberately narrow. Only the variants that carry a domain
/// meaning are translated; everything else becomes
/// [`ChainStoreError::Backend`], whose contract is that it is opaque and must
/// not be branched on. Inventing a domain meaning for an LMDB error would give
/// a consumer something to branch on that this backend cannot promise another
/// one would produce.
///
/// Narrow is not the same as lossy, though. The untranslated error is handed
/// over whole as the cause, so an operator reading the log still reaches the
/// LMDB errno underneath it. Rendering it with `to_string` instead would keep
/// only the top line and drop that error's own `source`, which is where the
/// actual failure usually is.
fn chain_store_error(error: StoreError) -> ChainStoreError {
    match error {
        StoreError::DataUnavailable(what) => ChainStoreError::MissingRow(what),
        StoreError::FeatureUnavailable(feature) => {
            ChainStoreError::Unavailable(capability_for_feature(feature))
        }
        other => ChainStoreError::backend_because(other.to_string(), other),
    }
}

/// Which domain capability a routing refusal was about.
///
/// The router refuses with a static feature name, which is this crate's
/// vocabulary; the domain's is coarser. Anything unrecognised maps to
/// [`StoreCapability::Core`], which is the conservative reading: a store that
/// cannot say which capability it lacks is reported as lacking the one every
/// store must have, so the caller routes elsewhere rather than retrying.
///
/// # Why the names come from `CapabilityRequest` rather than from literals
///
/// A routing refusal carries [`CapabilityRequest::name`], so matching on
/// hand-written literals lets producer and matcher drift: the two vocabularies
/// look alike enough that a mismatch reads as correct and every refusal
/// silently collapses to `Core`. Matching the constants the router itself
/// produces makes the drift impossible — a rename in `capability.rs` moves both
/// sides at once.
///
/// The lowercase arms are the second producer: `finalised_source` raises those
/// names directly rather than through a [`CapabilityRequest`], so they have no
/// constant to borrow and must be spelled out.
fn capability_for_feature(feature: &str) -> StoreCapability {
    const SPENT_OUTPUT_INDEX: &str = CapabilityRequest::SpentOutputIndex.name();
    const TXOUT_SET_INDEX: &str = CapabilityRequest::TxOutSetIndex.name();
    const TRANSPARENT_HIST_INDEX: &str = CapabilityRequest::TransparentHistIndex.name();

    match feature {
        SPENT_OUTPUT_INDEX | "spent_output_index" => StoreCapability::SpentOutputs,
        TXOUT_SET_INDEX | "txout_set_index" => StoreCapability::TxOutSet,
        TRANSPARENT_HIST_INDEX | "transparent_history" => StoreCapability::TransparentHistory,
        _ => StoreCapability::Core,
    }
}

/// A corrupt row, reported to the operator on its way to the caller.
///
/// # Why the logging is here rather than at each site
///
/// A corrupt row is the one read failure nothing upstream can act on. The
/// caller's recovery is to fall through to the validator, which is correct and
/// silent — so without a log here a store that is quietly rotting is
/// indistinguishable from one that is merely behind, for as long as the
/// validator keeps covering. The read path has no other place this surfaces:
/// the error is converted to a domain error, then to a status, and by then the
/// cause naming the field is gone.
///
/// Centralised so every conversion in this file reports identically and no new
/// one can be added that forgets to. `warn` rather than `error`: the request is
/// still answered, from elsewhere.
fn corrupt_row(expected: impl Into<String>) -> ChainStoreError {
    let error = ChainStoreError::corrupt_row(expected);
    report_corrupt_row(&error);
    error
}

/// A corrupt row whose rejecting conversion had a typed error to explain it.
fn corrupt_row_because(
    expected: impl Into<String>,
    cause: impl std::error::Error + Send + Sync + 'static,
) -> ChainStoreError {
    let error = ChainStoreError::corrupt_row_because(expected, cause);
    report_corrupt_row(&error);
    error
}

/// Logs and counts a corrupt row.
fn report_corrupt_row(error: &ChainStoreError) {
    tracing::warn!(
        error = error as &dyn std::error::Error,
        "chain store read a row it cannot decode"
    );
    #[cfg(feature = "prometheus")]
    metrics::counter!(crate::metric_names::DB_CORRUPT_ROWS_TOTAL).increment(1);
}

/// Which chunked read a duration belongs to.
///
/// A marker rather than the metric name itself, because `metric_names` is
/// behind the `prometheus` feature and naming a constant from it at the call
/// site would put a `cfg` on every read.
#[derive(Debug, Clone, Copy)]
enum ChunkRead {
    Stored,
    Compact,
}

/// How long a chunked read took.
///
/// A type rather than a bare `Instant` so the `prometheus` feature is handled
/// once: without it this compiles to nothing and no call site needs a `cfg`.
struct ReadTimer {
    #[cfg(feature = "prometheus")]
    started: std::time::Instant,
}

impl ReadTimer {
    fn start() -> Self {
        Self {
            #[cfg(feature = "prometheus")]
            started: std::time::Instant::now(),
        }
    }

    /// Records the elapsed time against `read`'s metric.
    ///
    /// Recorded whether the read succeeded or failed: a read that fails slowly
    /// is the symptom worth seeing, and dropping those samples would make a
    /// degrading store look faster as it got worse.
    fn record(self, read: ChunkRead) {
        #[cfg(feature = "prometheus")]
        {
            let metric = match read {
                ChunkRead::Stored => crate::metric_names::DB_BLOCK_READ_SECONDS,
                ChunkRead::Compact => crate::metric_names::DB_COMPACT_READ_SECONDS,
            };
            metrics::histogram!(metric).record(self.started.elapsed().as_secs_f64());
        }
        #[cfg(not(feature = "prometheus"))]
        let _ = read;
    }
}

/// A source failure, as the domain names it.
///
/// A validator failure is already the domain's, so it passes through untouched.
/// Anything else failed locally while committing, and is carried as the cause
/// for the same reason as in [`chain_store_error`].
fn chain_store_source_error(error: StoreError) -> ChainStoreSourceError {
    match error {
        StoreError::Source(source) => source,
        other => ChainStoreSourceError::commit_because(other.to_string(), other),
    }
}

// ---------------------------------------------------------------------------
// Height, hash and position
// ---------------------------------------------------------------------------

/// This crate's height, as the domain names it.
///
/// The stored height is any `u32`; the domain's is validated against the
/// protocol maximum. A stored height that cannot be expressed is a corrupt row,
/// not a caller error, so it surfaces as [`ChainStoreError::CorruptRow`] rather
/// than being clamped into a height that names a different block.
fn domain_height(height: Height) -> Result<DomainHeight, ChainStoreError> {
    DomainHeight::try_from(height.0).map_err(|error| {
        corrupt_row_because(format!("valid height for stored value {height}"), error)
    })
}

/// The domain's height, as this crate names it.
///
/// Infallible: every domain height is a `u32`.
fn stored_height(height: DomainHeight) -> Height {
    Height(u32::from(height))
}

/// The same 32 bytes, as the domain names them.
fn domain_hash(hash: BlockHash) -> DomainBlockHash {
    DomainBlockHash::from(hash.0)
}

/// The same 32 bytes, as this crate names them.
fn stored_hash(hash: DomainBlockHash) -> BlockHash {
    BlockHash(hash.into())
}

/// A height and hash together, or `None` if the height is not expressible.
pub(crate) fn domain_block_ref(height: Height, hash: BlockHash) -> Option<BlockRef> {
    DomainHeight::try_from(height.0)
        .ok()
        .map(|height| BlockRef {
            height,
            hash: domain_hash(hash),
        })
}

/// A stored transaction location, as a domain position.
fn block_tx_position(location: TxLocation) -> Result<BlockTxPosition, ChainStoreError> {
    Ok(BlockTxPosition {
        height: domain_height(Height(location.block_height()))?,
        tx_index: TxIndex::from(location.tx_index()),
    })
}

/// A domain position, as a stored transaction location.
///
/// `None` when the index exceeds what the stored form can hold. The stored
/// location keys a transaction by a `u16` index, so a position beyond that
/// names nothing on disk — which is an answer, not an error.
fn tx_location(position: BlockTxPosition) -> Option<TxLocation> {
    let tx_index = u16::try_from(position.tx_index).ok()?;
    Some(TxLocation::new(u32::from(position.height), tx_index))
}

/// The same 32 bytes and index, as this crate names them.
fn stored_outpoint(outpoint: &DomainOutpoint) -> Outpoint {
    Outpoint::new(outpoint.txid.into(), outpoint.index)
}

/// The same 32 bytes and index, as the domain names them.
///
/// Public for the reason [`stored_tx_out`] is: ChainIndex keys its cross-seam
/// UTXO fold by outpoint, and both halves must key it the same way.
pub fn domain_outpoint(outpoint: &Outpoint) -> DomainOutpoint {
    DomainOutpoint {
        txid: TransactionId::from(*outpoint.prev_txid()),
        index: outpoint.prev_index(),
    }
}

/// A stored txid, as the domain names it.
fn domain_txid(txid: TransactionHash) -> TransactionId {
    TransactionId::from(txid.0)
}

// ---------------------------------------------------------------------------
// Transparent outputs
// ---------------------------------------------------------------------------

/// A stored transparent output, as the domain names it.
///
/// The type tag is decoded rather than passed through: on disk it is a byte,
/// and a byte that names no script form is a corrupt row. Rejecting it here
/// keeps every domain-side consumer from having to consider a fourth case.
///
/// Public because ChainIndex folds the recent window onto the accumulator this
/// store hands it, and the recent window still arrives as [`IndexedBlock`]. It
/// therefore has to express a stored output as a domain one using the *same*
/// rule this crate uses, or the two halves of one commitment would disagree.
/// It goes private again when the recent window stops being expressed in this
/// crate's shapes.
pub fn stored_tx_out(output: &TxOutCompact) -> Result<StoredTxOut, ChainStoreError> {
    let script_type = match output.script_type_enum() {
        Some(crate::types::ScriptType::P2PKH) => ScriptType::P2PKH,
        Some(crate::types::ScriptType::P2SH) => ScriptType::P2SH,
        Some(crate::types::ScriptType::NonStandard) => ScriptType::NonStandard,
        None => {
            return Err(corrupt_row(format!(
                "valid script type for stored output tag {}",
                output.script_type()
            )))
        }
    };

    Ok(StoredTxOut {
        value: Zatoshis::new(output.value()).map_err(|error| {
            corrupt_row_because(
                format!("in-range value for stored output {}", output.value()),
                error,
            )
        })?,
        address: StoredAddress {
            hash: *output.script_hash(),
            script_type,
        },
    })
}

/// Every transparent output of a stored transaction.
fn stored_tx_outs(transparent: &TransparentCompactTx) -> Result<Vec<StoredTxOut>, ChainStoreError> {
    transparent.outputs().iter().map(stored_tx_out).collect()
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

/// A stored block, as the domain names it.
///
/// The header is reassembled from two stored pieces: the context, which carries
/// the identity an index reads (hash, parent, height), and the data, which
/// carries the consensus fields. They are separate on disk because they are
/// written to separate tables; nothing above this cares.
fn stored_block(block: IndexedBlock) -> Result<StoredBlock, ChainStoreError> {
    let context = &block.context;
    let data = &block.data;

    let header = BlockHeader {
        hash: domain_hash(*context.hash()),
        version: data.version,
        prev_hash: domain_hash(*context.parent_hash()),
        height: domain_height(context.height())?,
        time: u32::try_from(data.time).map_err(|error| {
            corrupt_row_because(
                format!("in-range block time for stored value {}", data.time),
                error,
            )
        })?,
        merkle_root: data.merkle_root.into(),
        block_commitments: data.block_commitments.into(),
        bits: data.bits.as_bits(),
        nonce: data.nonce,
        solution: match data.solution {
            crate::types::EquihashSolution::Standard(bytes) => {
                zaino_primitives::types::EquihashSolution::Standard(bytes)
            }
            crate::types::EquihashSolution::Regtest(bytes) => {
                zaino_primitives::types::EquihashSolution::Regtest(bytes)
            }
        },
    };

    Ok(StoredBlock {
        header,
        transactions: block
            .transactions
            .iter()
            .map(stored_compact_tx)
            .collect::<Result<Vec<_>, _>>()?,
        tree_roots: tree_roots(&block.commitment_tree_data),
        chainwork: domain_chainwork(context.chainwork()),
    })
}

/// A stored transaction, as the domain's compact transaction.
///
/// The coinbase's null prevout is kept, because it is what is on disk and
/// because dropping it here would make a block read disagree with a block
/// write about the same block. A consumer building a spend set skips it, as
/// the store's own spent index does.
fn stored_compact_tx(tx: &CompactTxData) -> Result<StoredTx, ChainStoreError> {
    let (sapling_value, orchard_value) = tx.balances();

    Ok(StoredTx {
        sapling_value: sapling_value.map(SignedZatoshis::new),
        orchard_value: orchard_value.map(SignedZatoshis::new),
        ironwood_value: tx.ironwood().value().map(SignedZatoshis::new),
        compact: stored_compact_tx_body(tx)?,
    })
}

/// The compact half of a stored transaction.
fn stored_compact_tx_body(tx: &CompactTxData) -> Result<PreIndexCompactTx, ChainStoreError> {
    let transparent = tx.transparent();

    Ok(PreIndexCompactTx {
        txid: domain_txid(*tx.txid()),
        transparent_inputs: transparent
            .inputs()
            .iter()
            .map(|input| TransparentInput {
                prev_txid: TransactionId::from(*input.prevout_txid()),
                prev_index: input.prevout_index(),
            })
            .collect(),
        transparent_outputs: transparent
            .outputs()
            .iter()
            .map(transparent_output)
            .collect::<Result<Vec<_>, _>>()?,
        sapling_nullifiers: tx
            .sapling()
            .spends()
            .iter()
            .map(|spend| Nullifier::from(*spend.nullifier()))
            .collect(),
        sapling_outputs: tx
            .sapling()
            .outputs()
            .iter()
            .map(|output| SaplingOutput {
                cmu: (*output.cmu()).into(),
                ephemeral_key: (*output.ephemeral_key()).into(),
                enc_ciphertext: EncryptedCiphertext::new(output.ciphertext().to_vec()),
            })
            .collect(),
        orchard_actions: tx.orchard().actions().iter().map(orchard_action).collect(),
        ironwood_actions: tx.ironwood().actions().iter().map(orchard_action).collect(),
    })
}

fn orchard_action(action: &crate::types::CompactOrchardAction) -> OrchardAction {
    OrchardAction {
        nullifier: (*action.nullifier()).into(),
        cmx: (*action.cmx()).into(),
        ephemeral_key: (*action.ephemeral_key()).into(),
        enc_ciphertext: EncryptedCiphertext::new(action.ciphertext().to_vec()),
    }
}

/// A stored output, as a domain transparent output.
///
/// The locking script is rebuilt from the address key. For P2PKH and P2SH that
/// is exact — a standard script is fully determined by its hash, which is why
/// storing the hash lost nothing.
///
/// # The non-standard case, and why it is not an empty script
///
/// A non-standard output's real script is gone: the store kept its first
/// twenty bytes as an index key and nothing else. What comes back is therefore
/// not the script, and cannot be. What it *must* be is something that
/// [`classify_script`](zaino_primitives::types::classify_script) maps back to
/// the same twenty bytes and the same classification, because this conversion's
/// inverse — [`indexed_block_from_stored`] — reclassifies whatever is here to
/// rebuild the row.
///
/// The 21-byte `tag ‖ hash` form does exactly that, and it is not invented for
/// the purpose: it is the shape those rows have always had on disk, and the
/// reason `classify_script` carries a 21-byte arm at all.
///
/// Returning an empty script instead — which is what this did — round-tripped
/// to an all-zero key, so a block read out of the store and written back put a
/// *different* address key on disk for every non-standard output. The genesis
/// coinbase is one, which is to say the very first block of every chain Zaino
/// indexes. Nothing detected it: both rows decode, both hash, and only the key
/// changes.
fn transparent_output(output: &TxOutCompact) -> Result<TransparentOutput, ChainStoreError> {
    let script_type = output.script_type_enum().ok_or_else(|| {
        corrupt_row(format!(
            "valid script type for stored output tag {}",
            output.script_type()
        ))
    })?;

    let script = crate::types::build_standard_script(*output.script_hash(), script_type)
        .unwrap_or_else(|| non_standard_key_script(output));

    Ok(TransparentOutput {
        value: Zatoshis::new(output.value()).map_err(|error| {
            corrupt_row_because(
                format!("in-range value for stored output {}", output.value()),
                error,
            )
        })?,
        script: Script::new(script),
    })
}

/// A non-standard output's index key, in the shape it is keyed from.
///
/// `tag ‖ hash`, so classifying it yields the key back unchanged. See
/// [`transparent_output`] for why this is a round-trip requirement rather than
/// a reconstruction.
fn non_standard_key_script(output: &TxOutCompact) -> Vec<u8> {
    let mut script = Vec::with_capacity(21);
    script.push(output.script_type());
    script.extend_from_slice(output.script_hash());
    script
}

/// The stored treestate, as the domain names it.
///
/// An all-zero sapling or orchard root reads back as `Some`, not `None`: on
/// disk those pools store a zero root where the domain would say "absent", and
/// the two are only distinguishable for ironwood, which stores an `Option`.
/// Reporting the zero root as present is what the stored bytes say, and
/// inventing an absence from a zero value would make a pre-activation block
/// indistinguishable from one with an empty tree.
/// Stored chainwork, as the domain's 256-bit big-endian value.
///
/// The store holds work as a `u128`, which is ample — Zcash's cumulative work
/// is nowhere near 2^128 — while the domain carries the 256-bit form the RPC
/// surface reports. Widening is left-padding with zeroes, and cannot lose
/// anything.
fn domain_chainwork(chainwork: &crate::types::ChainWork) -> DomainChainWork {
    let mut bytes = [0u8; 32];
    bytes[16..].copy_from_slice(&chainwork.as_non_zero_u128().get().to_be_bytes());
    DomainChainWork::new(bytes)
}

fn tree_roots(data: &CommitmentTreeData) -> TreeRoots {
    let roots = data.roots();
    let sizes = data.sizes();

    TreeRoots {
        sapling: Some(TreeRootInfo {
            root: (*roots.sapling()).into(),
            size: u64::from(sizes.sapling()),
        }),
        orchard: Some(TreeRootInfo {
            root: (*roots.orchard()).into(),
            size: u64::from(sizes.orchard()),
        }),
        ironwood: roots.ironwood().map(|root| TreeRootInfo {
            root: root.into(),
            size: u64::from(sizes.ironwood()),
        }),
    }
}

// ---------------------------------------------------------------------------
// Capabilities and schema
// ---------------------------------------------------------------------------

/// This backend's capability bits, as the domain's capability set.
///
/// Not one-for-one: the domain names indexes a consumer can ask about, where
/// the bits name trait surfaces this crate routes on. Several bits collapse —
/// a store either answers block reads or it does not, and which of the three
/// per-pool surfaces it used to get there is not a distinction a consumer can
/// act on.
///
/// # Why this is generic over the reader
///
/// Every capability is named as `<R as SomePort>::CAPABILITY` rather than as a
/// [`StoreCapability`] variant, so the advertisement is tied to the port that
/// answers it. Choosing variants by hand let the two drift in both directions:
/// a store could advertise an index it does not serve, or serve one it never
/// advertises, and both compile. Now a capability can only be advertised for a
/// port `R` actually implements — drop one of these impls and this stops
/// compiling instead of quietly over-promising.
///
/// It does not make the runtime bits agree with the compile-time impls; that
/// is what the bits are for, since this backend implements every port and
/// decides at runtime which it can serve. What it fixes is the mapping between
/// the two being restated here rather than read from the port.
fn store_capabilities<R>(capability: Capability) -> StoreCapabilities
where
    R: ChainStoreReader
        + StoredBlockRead
        + CompactBlockRead
        + TransactionIndex
        + SpentOutputIndex
        + TxOutSetIndex,
{
    let mut capabilities = vec![<R as ChainStoreReader>::CAPABILITY];

    // Stored blocks need the header, the txids and every per-pool surface: a
    // block missing one of them is not a block this store can hand over.
    if capability.has(
        Capability::CHAIN_BLOCK_EXT
            .union(Capability::BLOCK_CORE_EXT)
            .union(Capability::BLOCK_TRANSPARENT_EXT)
            .union(Capability::BLOCK_SHIELDED_EXT),
    ) {
        capabilities.push(<R as StoredBlockRead>::CAPABILITY);
    }
    if capability.has(Capability::COMPACT_BLOCK_EXT) {
        capabilities.push(<R as CompactBlockRead>::CAPABILITY);
    }
    if capability.has(Capability::BLOCK_CORE_EXT) {
        capabilities.push(<R as TransactionIndex>::CAPABILITY);
    }
    // Both halves: the domain's spent-output surface answers "what did this
    // outpoint hold" from the transparent rows as well as "who spent it" from
    // the spent index, and a store with only one of them cannot serve it.
    if capability.has(Capability::SPENT_OUTPUT_INDEX.union(Capability::BLOCK_TRANSPARENT_EXT)) {
        capabilities.push(<R as SpentOutputIndex>::CAPABILITY);
    }
    if capability.has(Capability::TXOUT_SET_INDEX) {
        capabilities.push(<R as TxOutSetIndex>::CAPABILITY);
    }
    // The one capability still named directly rather than read off its port.
    // `TransparentHistoryIndex` is implemented behind a feature, so it cannot
    // be a bound on this function in a build without it, and the advertisement
    // has to stand on its own. It is gated on the same bit either way.
    if capability.has(Capability::TRANSPARENT_HIST_INDEX) {
        capabilities.push(StoreCapability::TransparentHistory);
    }

    StoreCapabilities::new(capabilities)
}

/// The persisted metadata, as the domain's schema.
///
/// The on-disk record names the migration *phase* but not its destination,
/// because a migration always runs towards the version the running build
/// targets. So the target comes from the build rather than from disk, and a
/// database recorded mid-migration by an older build reports the version this
/// build would take it to.
fn store_schema(metadata: &DbMetadata) -> StoreSchema {
    let version = schema_version(metadata.version());
    let migration = match metadata.migration_status() {
        MigrationStatus::Empty | MigrationStatus::Complete => MigrationState::Settled,
        MigrationStatus::PartialBuidInProgress
        | MigrationStatus::PartialBuildComplete
        | MigrationStatus::FinalBuildInProgress => MigrationState::InProgress {
            from: version,
            to: schema_version(DB_VERSION_V1),
        },
    };

    StoreSchema { version, migration }
}

fn schema_version(version: crate::store::capability::DbVersion) -> SchemaVersion {
    SchemaVersion {
        major: version.major(),
        minor: version.minor(),
        patch: version.patch(),
    }
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

impl<T: ChainStoreSource> ChainStoreReader for DbReader<T> {
    fn watermark(&self) -> StoreWatermark {
        self.inner.watermark()
    }

    fn capabilities(&self) -> StoreCapabilities {
        store_capabilities::<Self>(self.inner.capability())
    }

    async fn schema(&self) -> Result<StoreSchema, ChainStoreError> {
        let metadata = self.get_metadata().await.map_err(chain_store_error)?;
        Ok(store_schema(&metadata))
    }

    async fn block_hash(
        &self,
        height: DomainHeight,
    ) -> Result<Option<DomainBlockHash>, ChainStoreError> {
        self.bounded(height)?;
        Ok(DbReader::get_block_hash(self, stored_height(height))
            .await
            .map_err(chain_store_error)?
            .map(domain_hash))
    }

    async fn block_height(
        &self,
        hash: DomainBlockHash,
    ) -> Result<Option<DomainHeight>, ChainStoreError> {
        match DbReader::get_block_height(self, stored_hash(hash))
            .await
            .map_err(chain_store_error)?
        {
            Some(height) => Ok(Some(domain_height(height)?)),
            None => Ok(None),
        }
    }

    fn status(&self) -> StatusType {
        DbReader::status(self)
    }
}

impl<T: ChainStoreSource> DbReader<T> {
    /// Rejects a height the store cannot answer for yet.
    ///
    /// Above the watermark is not a miss: the block probably exists and simply
    /// is not finalised, so a caller must route the question elsewhere rather
    /// than conclude the chain has no such block.
    fn bounded(&self, height: DomainHeight) -> Result<(), ChainStoreError> {
        let watermark = self.inner.watermark();
        if watermark.covers(height) {
            return Ok(());
        }
        // A passthrough store is not bounded by what it holds, because it is
        // not answering from what it holds: the read goes to the validator, and
        // the validator has the block. The watermark still describes the
        // durable rows — that is what it is for — but using it as a *limit*
        // here would refuse a question this store can answer perfectly well,
        // which is exactly the case a store that is still building is in.
        if watermark.provenance == zaino_chain_store::Provenance::Passthrough {
            return Ok(());
        }
        // No tip at all is not "above the watermark" — there is no watermark to
        // be above. A store still opening or still empty is transiently unable
        // to answer, which is what the caller needs to know.
        match watermark.tip {
            Some(tip) => Err(ChainStoreError::AboveWatermark {
                requested: height,
                watermark: tip.height,
            }),
            None => Err(ChainStoreError::NotReady),
        }
    }
}

/// How many blocks one read transaction covers.
///
/// The existing compact-block walk uses the same figure, and for the same
/// reason: it bounds how long a reader slot is held without making the
/// per-transaction cost dominate. Chunk boundaries carry no meaning — a
/// consumer must not read anything into where one ends.
const BLOCKS_PER_READ_TRANSACTION: u32 = 1024;

/// Splits `start..=end` into chunks and reads each one.
///
/// Sequential rather than concurrent: the chunks are contiguous and the
/// consumer wants them in order, so overlapping reads would buy nothing and
/// would hold several reader slots at once. The stream stops at the first
/// error — a range with a hole in the middle is not a range.
fn chunked<B, F, Fut>(
    range: Option<(Height, Height)>,
    read: F,
) -> impl futures::Stream<Item = Result<Vec<B>, ChainStoreError>> + Send
where
    B: Send + 'static,
    F: FnMut(Height, Height) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Vec<B>, ChainStoreError>> + Send + 'static,
{
    // `range` is `None` when the whole request sits above the watermark. The
    // empty case is folded in here rather than returned as
    // `stream::empty()` from the caller, because the port hands back one
    // opaque stream type and two arms returning different types could not
    // both be it.
    let (cursor, end) = match range {
        Some((start, end)) => (Some(start), end),
        None => (None, Height(0)),
    };
    // The reader travels in the unfold's state rather than being borrowed by
    // the closure: the future the closure returns outlives the call, so a
    // borrow would tie the stream's lifetime to this frame.
    futures::stream::try_unfold((cursor, read), move |(cursor, mut read)| async move {
        let Some(from) = cursor else {
            return Ok(None);
        };
        let to = Height(
            from.0
                .saturating_add(BLOCKS_PER_READ_TRANSACTION - 1)
                .min(end.0),
        );
        let chunk = read(from, to).await?;
        let next = (to.0 < end.0).then(|| Height(to.0 + 1));
        Ok(Some((chunk, (next, read))))
    })
}

impl<T: ChainStoreSource> DbReader<T> {
    /// Narrows `start..=end` to what this store can answer for.
    ///
    /// A range extending above the watermark is truncated rather than refused,
    /// so a consumer merging with the recent window asks both halves the same
    /// question and lets each answer what it holds. `None` means the whole range
    /// is above the watermark, which is an empty answer rather than an error —
    /// the other half has all of it.
    ///
    /// A range whose start is above its end is rejected: ranges are ascending,
    /// and a descending one is a caller mistake rather than an empty set.
    fn clamped_range(
        &self,
        start: DomainHeight,
        end: DomainHeight,
    ) -> Result<Option<(Height, Height)>, ChainStoreError> {
        if start > end {
            return Err(ChainStoreError::InvalidRange { start, end });
        }

        let watermark = self.inner.watermark();

        // Passthrough answers from the validator, so there is nothing to clamp
        // to — see [`Self::bounded`]. The ascending check above still applies,
        // because that one is about the request rather than about coverage.
        if watermark.provenance == zaino_chain_store::Provenance::Passthrough {
            return Ok(Some((stored_height(start), stored_height(end))));
        }

        let Some(tip) = watermark.tip else {
            return Err(ChainStoreError::NotReady);
        };
        if start > tip.height {
            return Ok(None);
        }
        Ok(Some((
            stored_height(start),
            stored_height(end.min(tip.height)),
        )))
    }
}

impl<T: ChainStoreSource> TransactionIndex for DbReader<T> {
    async fn tx_position(
        &self,
        txid: &TransactionId,
    ) -> Result<Option<BlockTxPosition>, ChainStoreError> {
        match self
            .get_tx_location(&TransactionHash((*txid).into()))
            .await
            .map_err(chain_store_error)?
        {
            Some(location) => Ok(Some(block_tx_position(location)?)),
            None => Ok(None),
        }
    }

    async fn txid_at(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<TransactionId>, ChainStoreError> {
        self.bounded(position.height)?;
        let Some(location) = tx_location(position) else {
            return Ok(None);
        };
        // The backend errors on a miss where the domain answers `None`: asking
        // about a position past the end of a block is a reasonable question.
        match self.get_txid(location).await {
            Ok(txid) => Ok(Some(domain_txid(txid))),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(error) => Err(chain_store_error(error)),
        }
    }
}

impl<T: ChainStoreSource> SpentOutputIndex for DbReader<T> {
    async fn outpoint_spenders(
        &self,
        outpoints: &[DomainOutpoint],
    ) -> Result<Vec<Option<SpenderRef>>, ChainStoreError> {
        let stored: Vec<Outpoint> = outpoints.iter().map(stored_outpoint).collect();
        let locations = DbReader::get_outpoint_spenders(self, stored)
            .await
            .map_err(chain_store_error)?;

        // The domain answer carries the spender's txid as well as its position,
        // because every caller resolves one to the other immediately. Doing it
        // here costs the same reads and halves the traffic across the seam.
        let mut spenders = Vec::with_capacity(locations.len());
        for location in locations {
            let Some(location) = location else {
                spenders.push(None);
                continue;
            };
            let txid = self.get_txid(location).await.map_err(chain_store_error)?;
            spenders.push(Some(SpenderRef {
                position: block_tx_position(location)?,
                txid: domain_txid(txid),
            }));
        }
        Ok(spenders)
    }

    async fn previous_outputs(
        &self,
        outpoints: &[DomainOutpoint],
    ) -> Result<Vec<Option<StoredTxOut>>, ChainStoreError> {
        let mut outputs = Vec::with_capacity(outpoints.len());
        for outpoint in outpoints {
            outputs.push(self.previous_output(outpoint).await?);
        }
        Ok(outputs)
    }

    async fn unspent_output(
        &self,
        outpoint: DomainOutpoint,
    ) -> Result<Option<StoredTxOut>, ChainStoreError> {
        let Some(output) = self.previous_output(&outpoint).await? else {
            return Ok(None);
        };
        let spent = DbReader::get_outpoint_spender(self, stored_outpoint(&outpoint))
            .await
            .map_err(chain_store_error)?
            .is_some();

        Ok((!spent).then_some(output))
    }

    async fn transparent_outputs(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<Vec<StoredTxOut>>, ChainStoreError> {
        self.bounded(position.height)?;
        let Some(location) = tx_location(position) else {
            return Ok(None);
        };
        match DbReader::get_transparent(self, location)
            .await
            .map_err(chain_store_error)?
        {
            Some(transparent) => Ok(Some(stored_tx_outs(&transparent)?)),
            None => Ok(None),
        }
    }
}

impl<T: ChainStoreSource> DbReader<T> {
    /// One outpoint's output, with a miss as `None`.
    ///
    /// The backend errors when the creating transaction is absent; the domain
    /// treats that as an answer, because an outpoint naming a transaction this
    /// store does not hold is exactly what a caller merging across the seam
    /// expects to see.
    async fn previous_output(
        &self,
        outpoint: &DomainOutpoint,
    ) -> Result<Option<StoredTxOut>, ChainStoreError> {
        match DbReader::get_previous_output(self, stored_outpoint(outpoint)).await {
            Ok(output) => Ok(Some(stored_tx_out(&output)?)),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(error) => Err(chain_store_error(error)),
        }
    }
}

impl<T: ChainStoreSource> TxOutSetIndex for DbReader<T> {
    async fn txout_set(&self) -> Result<TxOutSetAccumulator, ChainStoreError> {
        let accumulator = self
            .get_tx_out_set_info_accumulator()
            .await
            .map_err(chain_store_error)?;

        // The stored row *is* the domain value; only its encoding is this
        // backend's. The commitment it carries is defined by
        // `zaino_chain_store::txout_set`, which is also what maintains it — so
        // this hands the value over rather than restating it.
        Ok(accumulator.into_business())
    }
}

impl<T: ChainStoreSource> StoredBlockRead for DbReader<T> {
    #[tracing::instrument(skip(self), fields(start = %start, end = %end))]
    async fn blocks_chunk(
        &self,
        start: DomainHeight,
        end: DomainHeight,
    ) -> Result<Vec<StoredBlock>, ChainStoreError> {
        let Some((start, end)) = self.clamped_range(start, end)? else {
            return Ok(Vec::new());
        };

        // Timed around the chunk rather than the block: one read transaction
        // covers the range, so a per-block figure would divide one duration by
        // a count rather than measure anything.
        let read = ReadTimer::start();

        let blocks: Result<Vec<_>, _> = self
            .get_chain_block_range(start, end)
            .await
            .map_err(chain_store_error)?
            .into_iter()
            .map(stored_block)
            .collect();

        read.record(ChunkRead::Stored);
        blocks
    }

    async fn blocks_stream(
        &self,
        start: DomainHeight,
        end: DomainHeight,
    ) -> Result<
        impl futures::Stream<Item = Result<Vec<StoredBlock>, ChainStoreError>> + Send + use<T>,
        ChainStoreError,
    > {
        let reader = self.clone();
        Ok(chunked(self.clamped_range(start, end)?, move |from, to| {
            let reader = reader.clone();
            async move {
                reader
                    .get_chain_block_range(from, to)
                    .await
                    .map_err(chain_store_error)?
                    .into_iter()
                    .map(stored_block)
                    .collect()
            }
        }))
    }
}

impl<T: ChainStoreSource> CompactBlockRead for DbReader<T> {
    #[tracing::instrument(skip(self, pools), fields(start = %start, end = %end))]
    async fn compact_chunk(
        &self,
        start: DomainHeight,
        end: DomainHeight,
        pools: PoolFilter,
    ) -> Result<Vec<CompactBlock>, ChainStoreError> {
        let Some((start, end)) = self.clamped_range(start, end)? else {
            return Ok(Vec::new());
        };

        // The wallet-sync hot path: a syncing wallet spends almost all of its
        // time here, so this is the read whose latency a dashboard needs.
        let read = ReadTimer::start();

        let blocks = DbReader::get_compact_block_range(self, start, end, pools)
            .await
            .map_err(chain_store_error);

        read.record(ChunkRead::Compact);
        blocks
    }

    async fn compact_stream(
        &self,
        start: DomainHeight,
        end: DomainHeight,
        pools: PoolFilter,
    ) -> Result<
        impl futures::Stream<Item = Result<Vec<CompactBlock>, ChainStoreError>> + Send + use<T>,
        ChainStoreError,
    > {
        let reader = self.clone();
        Ok(chunked(self.clamped_range(start, end)?, move |from, to| {
            let reader = reader.clone();
            async move {
                DbReader::get_compact_block_range(&reader, from, to, pools)
                    .await
                    .map_err(chain_store_error)
            }
        }))
    }
}

impl<T: ChainStoreSource> ChainStoreService for FinalisedState<T> {
    type Reader = DbReader<T>;

    fn reader(&self) -> Self::Reader {
        FinalisedState::reader(self)
    }

    fn status(&self) -> StatusType {
        FinalisedState::status(self)
    }

    fn subscribe_watermark(&self) -> tokio::sync::watch::Receiver<StoreWatermark> {
        self.subscribe_watermark()
    }
}

impl<T: ChainStoreSource> ChainStoreIngest for FinalisedState<T> {
    async fn build_to(&self, target: DomainHeight) -> Result<(), ChainStoreSourceError> {
        FinalisedState::build_to(self, stored_height(target))
            .await
            .map_err(chain_store_source_error)
    }

    async fn rewind_to(&self, height: DomainHeight) -> Result<(), ChainStoreError> {
        FinalisedState::rewind_to(self, stored_height(height))
            .await
            .map_err(chain_store_error)
    }

    fn wait_until_built(&self) -> impl Future<Output = ()> + Send {
        self.wait_until_synced()
    }

    async fn shutdown(&self) -> Result<(), ChainStoreError> {
        FinalisedState::shutdown(self)
            .await
            .map_err(chain_store_error)
    }
}

// ---------------------------------------------------------------------------
// Freeze
// ---------------------------------------------------------------------------

/// A domain block, as the shape the writer takes.
///
/// The reverse of `stored_block`, and not quite its inverse: a block that
/// arrives from a composer was built from a validator's, so its transparent
/// outputs carry real locking scripts, which are classified here exactly as the
/// source-driven build path classifies them. A block that made the round trip
/// out of this store carries reconstructed scripts, which classify back to the
/// same key — so both origins produce the same rows.
///
/// Public because ChainIndex reads blocks through
/// [`zaino_chain_store::StoredBlockRead`] and still answers
/// its callers in [`IndexedBlock`], which is also the shape its chain-head
/// adapter produces. One conversion serving both directions is what keeps a
/// block from changing shape as it crosses the finalised seam. It goes private
/// again when `IndexedBlock` stops being ChainIndex's block.
pub fn indexed_block_from_stored(block: &StoredBlock) -> Result<IndexedBlock, ChainStoreError> {
    let header = &block.header;
    let hash = stored_hash(header.hash);

    let context = crate::types::BlockContext::new(
        hash,
        stored_hash(header.prev_hash),
        stored_chainwork(block.chainwork, hash)?,
        Height(u32::from(header.height)),
    );

    // Shared with the write direction rather than restated: both start from the
    // same `BlockHeader`, so a second copy would be the same mapping free to
    // drift — and had already drifted, this side stringifying the difficulty
    // failure the other keeps typed.
    //
    // A header that will not convert came off disk, so it is a corrupt row
    // rather than a backend failure, and the conversion's own error is carried
    // as the cause: it names which field was rejected and why, which is what
    // separates a corrupt row from a block this build cannot yet parse.
    let data = crate::conversion::block_data(header).map_err(|error| {
        corrupt_row_because(format!("a convertible header for block {hash}"), error)
    })?;

    let transactions = block
        .transactions
        .iter()
        .enumerate()
        .map(|(index, tx)| stored_compact_tx_data(index, tx, hash))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(IndexedBlock::new(
        context,
        data,
        transactions,
        commitment_tree_data(&block.tree_roots, hash)?,
    ))
}

/// The domain's 256-bit chainwork, as the width the store records.
///
/// Rejects anything above 2^128 rather than truncating. Zcash's cumulative work
/// is nowhere near that, so a value that does not fit did not come from this
/// chain, and a truncated one would put a lower chainwork on disk than the
/// block actually has — which reorders the chain.
fn stored_chainwork(
    chainwork: DomainChainWork,
    hash: BlockHash,
) -> Result<crate::types::ChainWork, ChainStoreError> {
    let bytes = <[u8; 32]>::from(chainwork);
    let (high, low) = bytes.split_at(16);

    if high.iter().any(|byte| *byte != 0) {
        return Err(ChainStoreError::backend(format!(
            "block {hash} has chainwork above what the store records"
        )));
    }

    let mut value = [0u8; 16];
    value.copy_from_slice(low);
    core::num::NonZeroU128::new(u128::from_be_bytes(value))
        .map(crate::types::ChainWork::new)
        .ok_or_else(|| ChainStoreError::backend(format!("block {hash} has zero chainwork")))
}

/// One domain transaction, as the shape the writer stores.
///
/// The transaction at index 0 gets the coinbase's null prevout if it does not
/// already carry one. A block from a validator has had it dropped; a block read
/// back out of this store still has it. Both must produce the same rows,
/// because the input is a persisted field.
fn stored_compact_tx_data(
    index: usize,
    stored: &StoredTx,
    block: BlockHash,
) -> Result<CompactTxData, ChainStoreError> {
    let tx = &stored.compact;
    let mut inputs: Vec<crate::types::TxInCompact> = Vec::new();
    let carries_null_prevout = tx
        .transparent_inputs
        .first()
        .is_some_and(|input| <[u8; 32]>::from(input.prev_txid) == [0u8; 32]);
    if index == 0 && !carries_null_prevout {
        inputs.push(crate::types::TxInCompact::null_prevout());
    }
    inputs.extend(
        tx.transparent_inputs
            .iter()
            .map(|input| crate::types::TxInCompact::new(input.prev_txid.into(), input.prev_index)),
    );

    let outputs = tx
        .transparent_outputs
        .iter()
        .map(|output| {
            let script: Vec<u8> = output.script.clone().into();
            let (hash, script_type) = zaino_primitives::types::classify_script(&script);
            crate::types::TxOutCompact::new(
                u64::from(output.value),
                hash,
                stored_script_tag(script_type),
            )
            .ok_or_else(|| {
                ChainStoreError::backend(format!(
                    "block {block} has a transparent output that cannot be stored"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CompactTxData::new(
        index as u64,
        TransactionHash(tx.txid.into()),
        TransparentCompactTx::new(inputs, outputs),
        crate::types::SaplingCompactTx::new(
            stored.sapling_value.map(i64::from),
            tx.sapling_nullifiers
                .iter()
                .map(|nullifier| crate::types::CompactSaplingSpend::new((*nullifier).into()))
                .collect(),
            tx.sapling_outputs
                .iter()
                .map(|output| {
                    crate::types::CompactSaplingOutput::new(
                        output.cmu.into(),
                        output.ephemeral_key.into(),
                        ciphertext_prefix(&output.enc_ciphertext),
                    )
                })
                .collect(),
        ),
        stored_orchard(stored.orchard_value, &tx.orchard_actions),
        stored_orchard(stored.ironwood_value, &tx.ironwood_actions),
    ))
}

/// One shielded pool's stored compact form.
///
/// Takes the value balance rather than defaulting it: it is a persisted field,
/// and a `None` written where the row held a balance is a row this store
/// rewrote while only meaning to read it.
fn stored_orchard(
    value: Option<SignedZatoshis>,
    actions: &[OrchardAction],
) -> crate::types::OrchardCompactTx {
    crate::types::OrchardCompactTx::new(
        value.map(i64::from),
        actions
            .iter()
            .map(|action| {
                crate::types::CompactOrchardAction::new(
                    action.nullifier.into(),
                    action.cmx.into(),
                    action.ephemeral_key.into(),
                    ciphertext_prefix(&action.enc_ciphertext),
                )
            })
            .collect(),
    )
}

/// The 52-byte scanning prefix, zero-padded if the source supplied less.
fn ciphertext_prefix(ciphertext: &EncryptedCiphertext) -> [u8; 52] {
    let bytes: Vec<u8> = ciphertext.clone().into();
    let mut prefix = [0u8; 52];
    let usable = bytes.len().min(52);
    prefix[..usable].copy_from_slice(&bytes[..usable]);
    prefix
}

fn stored_script_tag(script_type: ScriptType) -> u8 {
    match script_type {
        ScriptType::P2PKH => crate::types::ScriptType::P2PKH as u8,
        ScriptType::P2SH => crate::types::ScriptType::P2SH as u8,
        ScriptType::NonStandard => crate::types::ScriptType::NonStandard as u8,
    }
}

/// The domain treestate, as the shape the writer stores.
///
/// Delegates to [`crate::conversion::commitment_tree_data`] rather than
/// repeating the field mapping. That matters for one field in particular: a
/// tree size that does not fit the stored width is *refused* there. A second
/// copy here narrowed it with a cast, which put a wrong size on disk for a
/// block whose real size nothing downstream re-derives — a silent wrong answer
/// on the write path, and exactly the drift a single definition prevents.
fn commitment_tree_data(
    roots: &TreeRoots,
    hash: BlockHash,
) -> Result<CommitmentTreeData, ChainStoreError> {
    crate::conversion::commitment_tree_data(roots, hash).map_err(|error| {
        ChainStoreError::backend(format!("block {hash} has an unstorable treestate: {error}"))
    })
}

impl<T: ChainStoreSource> ChainStoreFreezeSink for FinalisedState<T> {
    /// Writes blocks the composer has already seen fall beyond reorg.
    ///
    /// Idempotent on `(height, hash)` by delegation: the writer's put is a
    /// byte-compare on conflict, so re-seeing a block it already holds is a
    /// no-op and re-seeing a *different* block at the same height is an error
    /// rather than a silent overwrite. That is the property the freeze stream
    /// needs, because it can deliver the same heights twice across a reorg.
    ///
    /// Blocks below the store's tip are skipped rather than rejected. The
    /// stream has a retention window in which a block is both emitted and still
    /// held by the chain head, so a store that built past it through its own
    /// source will legitimately be handed blocks it already has.
    ///
    /// A gap is not repaired here. The writer is append-only and contiguous, so
    /// a block above `tip + 1` cannot be written; it is left for the
    /// source-driven build path, which is why that path cannot be removed.
    async fn freeze(&self, blocks: &[StoredBlock]) -> Result<(), ChainStoreError> {
        for block in blocks {
            let expected = match self.db_height().await.map_err(chain_store_error)? {
                Some(tip) => tip.0.saturating_add(1),
                None => crate::types::GENESIS_HEIGHT.0,
            };

            let height = u32::from(block.header.height);
            if height < expected {
                continue;
            }
            if height > expected {
                break;
            }

            self.write_block(indexed_block_from_stored(block)?)
                .await
                .map_err(chain_store_error)?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Transparent address history
// ---------------------------------------------------------------------------

/// What the finalised range shows happening to a set of addresses.
///
/// Assembled from the address index plus the transparent rows, rather than
/// served from a single table: the index records *which transactions touched an
/// address*, and the domain answer needs what those transactions did, which
/// means reading the transaction. That is also why spends carry the output they
/// spent — resolving each input's previous output is work the store has to do
/// anyway to decide whether the input belongs to the address, so handing the
/// result over costs nothing and saves the consumer a second round trip.
#[cfg(feature = "transparent_address_history_experimental")]
impl<T: ChainStoreSource> zaino_chain_store::TransparentHistoryIndex for DbReader<T> {
    async fn address_effects(
        &self,
        query: &zaino_chain_store::TransparentHistoryQuery,
    ) -> Result<zaino_chain_store::StoreAddressEffects, ChainStoreError> {
        use zaino_chain_store::{LocatedOutput, LocatedSpend, StoreAddressEffects};

        if query.start > query.end {
            return Err(ChainStoreError::InvalidRange {
                start: query.start,
                end: query.end,
            });
        }
        let Some((start, end)) = self.clamped_range(query.start, query.end)? else {
            return Ok(StoreAddressEffects::default());
        };

        let mut effects = StoreAddressEffects::default();

        for address in &query.addresses {
            let key =
                crate::types::AddrScript::new(address.hash, stored_script_tag(address.script_type));
            let Some(locations) = self
                .addr_tx_locations_by_range(key, start, end)
                .await
                .map_err(chain_store_error)?
            else {
                continue;
            };

            for location in locations {
                let position = block_tx_position(location)?;
                let txid = domain_txid(self.get_txid(location).await.map_err(chain_store_error)?);
                let Some(transparent) = DbReader::get_transparent(self, location)
                    .await
                    .map_err(chain_store_error)?
                else {
                    continue;
                };

                // Outputs this transaction created that pay the address.
                for (index, output) in transparent.outputs().iter().enumerate() {
                    if !pays(output, address) {
                        continue;
                    }
                    effects.outputs.push(LocatedOutput {
                        outpoint: DomainOutpoint {
                            txid,
                            index: index as u32,
                        },
                        output: stored_tx_out(output)?,
                        position,
                        txid,
                    });
                }

                // Inputs this transaction spent that belonged to the address.
                //
                // The coinbase's null prevout spends nothing, and
                // `spent_outpoints` already drops it — the same filter the
                // store's own spend index applies.
                for outpoint in transparent.spent_outpoints() {
                    let Some(previous) = self.previous_output_row(outpoint).await? else {
                        continue;
                    };
                    if !pays(&previous, address) {
                        continue;
                    }
                    effects.spends.push(LocatedSpend {
                        outpoint: DomainOutpoint {
                            txid: TransactionId::from(*outpoint.prev_txid()),
                            index: outpoint.prev_index(),
                        },
                        output: stored_tx_out(&previous)?,
                        position,
                        txid,
                    });
                }
            }
        }

        Ok(effects)
    }
}

/// Whether a stored output is keyed under `address`.
#[cfg(feature = "transparent_address_history_experimental")]
fn pays(output: &TxOutCompact, address: &StoredAddress) -> bool {
    *output.script_hash() == address.hash
        && output.script_type() == stored_script_tag(address.script_type)
}

#[cfg(feature = "transparent_address_history_experimental")]
impl<T: ChainStoreSource> DbReader<T> {
    /// The stored output an outpoint names, with a miss as `None`.
    async fn previous_output_row(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxOutCompact>, ChainStoreError> {
        match DbReader::get_previous_output(self, outpoint).await {
            Ok(output) => Ok(Some(output)),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(error) => Err(chain_store_error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The concrete reader whose ports the capability mapping is read from.
    type Reader = DbReader<zaino_source_zebra::ZebraValidator>;

    /// The capabilities a store on these bits advertises.
    fn advertised(capability: Capability) -> StoreCapabilities {
        store_capabilities::<Reader>(capability)
    }

    /// A corrupt row is reported to the operator, not only to the caller.
    ///
    /// The caller's recovery is to fall through to the validator, which is
    /// silent by design — so this log is the only thing that distinguishes a
    /// store that is rotting from one that is merely behind. Asserted through
    /// a subscriber rather than by reading the code, because the reporting is
    /// a side effect and nothing else would notice it being dropped.
    #[test]
    fn a_corrupt_row_is_reported_to_the_operator() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("capture buffer mutex poisoned")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for Captured {
            type Writer = Self;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let _ = domain_height(Height(u32::MAX));
        });

        let logged = String::from_utf8(
            captured
                .0
                .lock()
                .expect("capture buffer mutex poisoned")
                .clone(),
        )
        .expect("log output is utf-8");

        assert!(logged.contains("WARN"), "not logged at warn: {logged:?}");
        assert!(
            logged.contains("cannot decode"),
            "the corrupt row was not reported: {logged:?}"
        );
    }

    /// A stored value the domain cannot express is corruption, not absence.
    ///
    /// The row is present and readable; what is wrong is inside it. Reporting
    /// that as `MissingRow` — which means an index points at a row that is not
    /// there — sends an operator to rebuild an index when what they need is to
    /// refetch and rewrite the row.
    #[test]
    fn an_unrepresentable_stored_value_is_a_corrupt_row() {
        use std::error::Error as _;

        let error = domain_height(Height(u32::MAX)).expect_err("above the protocol maximum");

        let ChainStoreError::CorruptRow { ref cause, .. } = error else {
            panic!("a present row holding a bad value is corrupt, not missing: {error:?}");
        };
        assert!(
            cause.is_some(),
            "the conversion knows which bound was exceeded; that should survive"
        );
        assert!(error.source().is_some());
    }

    /// A stored amount above the money supply is a corrupt row too.
    ///
    /// The other reachable shape of the same fault: `TxOutCompact` holds any
    /// `u64`, while the domain's `Zatoshis` is bounded by the supply, so this
    /// one is constructible where the script-type arms are not — those are
    /// refused by both `TxOutCompact::new` and its decoder, and are defensive.
    #[test]
    fn a_stored_amount_above_the_supply_is_a_corrupt_row() {
        use std::error::Error as _;

        let output = TxOutCompact::new(u64::MAX, [0u8; 20], 0).expect("a valid script type");

        for error in [
            transparent_output(&output).expect_err("above the money supply"),
            stored_tx_out(&output).expect_err("above the money supply"),
        ] {
            assert!(
                matches!(error, ChainStoreError::CorruptRow { .. }),
                "a present row holding a bad amount is corrupt, not missing: {error:?}"
            );
            assert!(
                error.source().is_some(),
                "the overflow names the bound it exceeded; that should survive"
            );
        }
    }

    /// An untranslated backend failure reaches the domain with its cause.
    ///
    /// The boundary is meant to be opaque to *branching*, not to reading. An
    /// earlier version rendered the error with `to_string`, which kept the top
    /// line and dropped the error's own `source` — so an operator logging the
    /// chain got the summary and none of the LMDB detail underneath it.
    #[test]
    fn an_untranslated_failure_carries_its_cause() {
        use std::error::Error as _;

        let error = chain_store_error(StoreError::LmdbError(lmdb::Error::Panic));

        let ChainStoreError::Backend { ref message, .. } = error else {
            panic!("an LMDB failure has no domain meaning, so it must be Backend");
        };
        assert!(message.contains("LMDB"), "message was {message:?}");

        let cause = error.source().expect("the backend error must be carried");
        assert!(
            cause.to_string().contains("MDB_PANIC"),
            "cause was {cause:?}, which does not name the LMDB failure"
        );
    }

    /// A commit failure carries its cause the same way.
    #[test]
    fn a_failed_commit_carries_its_cause() {
        use std::error::Error as _;

        let error = chain_store_source_error(StoreError::LmdbError(lmdb::Error::Panic));

        assert!(matches!(error, ChainStoreSourceError::Commit { .. }));
        assert!(error
            .source()
            .expect("the backend error must be carried")
            .to_string()
            .contains("MDB_PANIC"));
    }

    /// A validator failure is already the domain's, so it is not re-wrapped.
    #[test]
    fn a_validator_failure_passes_through() {
        let error = chain_store_source_error(StoreError::Source(
            ChainStoreSourceError::unavailable("no route to validator"),
        ));

        assert!(matches!(error, ChainStoreSourceError::Unavailable { .. }));
    }

    /// Every port this backend claims to implement is actually implemented.
    ///
    /// A bound check, not a behaviour test, and it earns its place: nothing else
    /// notices when a port is declared in the domain crate and never satisfied
    /// here. The failure it catches is a consumer being unable to name a
    /// capability the store advertises — which shows up at wiring time in
    /// another crate, a long way from the cause.
    #[test]
    fn the_backend_satisfies_every_port_it_advertises() {
        fn reader<R: ChainStoreReader + StoredBlockRead + CompactBlockRead>() {}
        fn indexes<R: TransactionIndex + SpentOutputIndex + TxOutSetIndex>() {}
        fn service<S: ChainStoreService + ChainStoreIngest + ChainStoreFreezeSink>() {}
        #[cfg(feature = "transparent_address_history_experimental")]
        fn history<R: zaino_chain_store::TransparentHistoryIndex>() {}

        type Validator = zaino_source_zebra::ZebraValidator;

        reader::<DbReader<Validator>>();
        indexes::<DbReader<Validator>>();
        service::<FinalisedState<Validator>>();
        #[cfg(feature = "transparent_address_history_experimental")]
        history::<DbReader<Validator>>();
    }

    /// The capability mapping reports what a consumer can actually ask for.
    ///
    /// The domain names indexes; the bits name trait surfaces. Several bits
    /// collapse into one domain capability, and two of them collapse into
    /// `SpentOutputs` together — a store with the spent index but no
    /// transparent rows cannot answer "what did this outpoint hold", so
    /// claiming the capability would be a lie a caller only discovers by
    /// asking.
    #[test]
    fn a_store_missing_transparent_rows_does_not_claim_spent_outputs() {
        let without_rows = Capability::SPENT_OUTPUT_INDEX;
        assert!(!advertised(without_rows).contains(StoreCapability::SpentOutputs));

        let with_rows = Capability::SPENT_OUTPUT_INDEX | Capability::BLOCK_TRANSPARENT_EXT;
        assert!(advertised(with_rows).contains(StoreCapability::SpentOutputs));
    }

    /// Core is always claimed, even by a store that advertises nothing.
    ///
    /// Not a convenience: the domain's contract is that a store which cannot
    /// answer the core reads is not a store, so a consumer is entitled to
    /// assume the capability is present rather than check for it.
    #[test]
    fn core_is_always_claimed() {
        assert!(advertised(Capability::empty()).contains(StoreCapability::Core));
    }

    /// What a store advertises is exactly what its ports say it can answer.
    ///
    /// Every capability the mapping can produce is one of the ports' own
    /// `CAPABILITY` values, and every read port this backend implements is
    /// reachable through the mapping. The set is closed, so both directions can
    /// be checked rather than assumed: a port added to the domain without a bit
    /// to advertise it, or a bit advertising a capability no port answers,
    /// fails here.
    #[test]
    fn every_advertised_capability_belongs_to_a_port_this_backend_implements() {
        let everything = advertised(Capability::all());

        let ports = [
            <Reader as ChainStoreReader>::CAPABILITY,
            <Reader as StoredBlockRead>::CAPABILITY,
            <Reader as CompactBlockRead>::CAPABILITY,
            <Reader as TransactionIndex>::CAPABILITY,
            <Reader as SpentOutputIndex>::CAPABILITY,
            <Reader as TxOutSetIndex>::CAPABILITY,
        ];

        for capability in everything.iter() {
            #[cfg(not(feature = "transparent_address_history_experimental"))]
            assert!(
                ports.contains(&capability) || capability == StoreCapability::TransparentHistory,
                "{capability} is advertised but answered by no port"
            );
            #[cfg(feature = "transparent_address_history_experimental")]
            assert!(
                ports.contains(&capability)
                    || capability
                        == <Reader as zaino_chain_store::TransparentHistoryIndex>::CAPABILITY,
                "{capability} is advertised but answered by no port"
            );
        }

        for port in ports {
            assert!(
                everything.contains(port),
                "{port} is implemented but nothing advertises it"
            );
        }
    }

    /// Chainwork survives the round trip through the domain's wider form.
    ///
    /// The store records work as a `u128` and the domain as 256 bits. Widening
    /// is padding, and narrowing rejects rather than truncates — a truncated
    /// chainwork would be *lower* than the block's real work, which reorders
    /// the chain rather than failing.
    #[test]
    fn chainwork_widens_and_narrows_without_loss() {
        let hash = BlockHash([0u8; 32]);
        for raw in [1u128, 42, u64::MAX as u128, u128::MAX] {
            let stored =
                crate::types::ChainWork::new(core::num::NonZeroU128::new(raw).expect("non-zero"));
            let widened = domain_chainwork(&stored);
            let narrowed = stored_chainwork(widened, hash).expect("round trip");
            assert_eq!(narrowed.as_non_zero_u128().get(), raw);
        }
    }

    /// Chainwork the store cannot record is refused, not truncated.
    #[test]
    fn chainwork_above_the_stored_width_is_refused() {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        let error = stored_chainwork(DomainChainWork::new(bytes), BlockHash([0u8; 32]))
            .expect_err("above u128 must be refused");
        assert!(matches!(error, ChainStoreError::Backend { .. }));
    }

    /// A position past what the stored form can key is an answer, not an error.
    ///
    /// The store keys a transaction by a `u16` index where the domain uses a
    /// `u32`, so a position beyond that names nothing on disk. Asking about it
    /// is a reasonable question with the answer "nothing there".
    #[test]
    fn a_position_beyond_the_stored_index_width_names_nothing() {
        let height = DomainHeight::try_from(1).expect("valid height");
        assert!(tx_location(BlockTxPosition {
            height,
            tx_index: u32::from(u16::MAX),
        })
        .is_some());
        assert!(tx_location(BlockTxPosition {
            height,
            tx_index: u32::from(u16::MAX) + 1,
        })
        .is_none());
    }

    /// A treestate the stored width cannot hold is refused, not narrowed.
    ///
    /// Regression test. This conversion used to carry its own copy of the field
    /// mapping, whose tree-size step was an `as u32` — so a size above the
    /// stored width was written to disk narrowed, on the *write* path, for a
    /// block whose real size nothing downstream re-derives. It now delegates to
    /// the one mapping that rejects, and this pins that it still does.
    #[test]
    fn a_treestate_the_store_cannot_hold_is_refused() {
        use zaino_primitives::types::TreeRoot;

        let hash = BlockHash([7u8; 32]);
        let oversized = TreeRoots {
            sapling: Some(TreeRootInfo {
                root: TreeRoot::from([0u8; 32]),
                size: u64::from(u32::MAX) + 1,
            }),
            orchard: None,
            ironwood: None,
        };

        assert!(matches!(
            commitment_tree_data(&oversized, hash),
            Err(ChainStoreError::Backend { .. })
        ));

        // The same treestate one below the boundary is accepted, so the
        // rejection is about the width and not about the field being present.
        let representable = TreeRoots {
            sapling: Some(TreeRootInfo {
                root: TreeRoot::from([0u8; 32]),
                size: u64::from(u32::MAX),
            }),
            orchard: None,
            ironwood: None,
        };
        assert!(commitment_tree_data(&representable, hash).is_ok());
    }

    /// A routing refusal names the capability the caller was denied.
    ///
    /// Fed from [`CapabilityRequest::name`] rather than from hand-written
    /// strings, because the router is what produces these names and a test that
    /// invents its own cannot see the two vocabularies drift apart. An earlier
    /// version of this test passed against literals the router never emits
    /// while every real refusal collapsed to `Core`.
    #[test]
    fn a_routing_refusal_maps_to_the_capability_it_denied() {
        assert_eq!(
            capability_for_feature(CapabilityRequest::SpentOutputIndex.name()),
            StoreCapability::SpentOutputs
        );
        assert_eq!(
            capability_for_feature(CapabilityRequest::TxOutSetIndex.name()),
            StoreCapability::TxOutSet
        );
        assert_eq!(
            capability_for_feature(CapabilityRequest::TransparentHistIndex.name()),
            StoreCapability::TransparentHistory
        );
    }

    /// The names `finalised_source` raises directly map too.
    ///
    /// A second producer, which does not route through [`CapabilityRequest`]
    /// and so spells its features in its own lowercase vocabulary. It is the
    /// path that happened to match while the router's did not, so it gets its
    /// own test rather than sharing one.
    #[test]
    fn a_direct_refusal_maps_to_the_capability_it_denied() {
        assert_eq!(
            capability_for_feature("spent_output_index"),
            StoreCapability::SpentOutputs
        );
        assert_eq!(
            capability_for_feature("txout_set_index"),
            StoreCapability::TxOutSet
        );
        assert_eq!(
            capability_for_feature("transparent_history"),
            StoreCapability::TransparentHistory
        );
    }

    /// An unrecognised name falls back to `Core`.
    ///
    /// Rather than to the capability that happens to be nearest: over-claiming
    /// which index is missing would send a consumer to reroute a query that
    /// would have worked. `READ_CORE` is a real router name that lands here
    /// legitimately; the other is a name no producer emits.
    #[test]
    fn an_unrecognised_refusal_falls_back_to_core() {
        assert_eq!(
            capability_for_feature(CapabilityRequest::ReadCore.name()),
            StoreCapability::Core
        );
        assert_eq!(
            capability_for_feature("no_such_feature"),
            StoreCapability::Core
        );
    }

    /// A migrating store reports where it is going, not just where it is.
    ///
    /// The on-disk record names the phase but not the destination, because a
    /// migration always runs towards the version the running build targets. So
    /// the target comes from the build.
    #[test]
    fn a_migrating_store_reports_its_target() {
        let settled = store_schema(&DbMetadata::new(
            DB_VERSION_V1,
            [0u8; 32],
            MigrationStatus::Empty,
        ));
        assert_eq!(settled.migration, MigrationState::Settled);

        let from = crate::store::capability::DbVersion::new(1, 1, 0);
        let migrating = store_schema(&DbMetadata::new(
            from,
            [0u8; 32],
            MigrationStatus::FinalBuildInProgress,
        ));
        assert_eq!(
            migrating.migration,
            MigrationState::InProgress {
                from: schema_version(from),
                to: schema_version(DB_VERSION_V1),
            }
        );
    }
}
