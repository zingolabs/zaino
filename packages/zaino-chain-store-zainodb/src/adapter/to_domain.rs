//! Reading: what is on disk, as the domain names it.
//!
//! The conversions a read takes on its way out — heights, hashes, positions,
//! outputs, blocks, and the store's own capability and schema. Every one of
//! them can fail on a value the domain cannot express, which is corruption
//! rather than absence, and each says so.
//!
//! The opposite direction is `from_domain`. They are separated because
//! they fail differently: a value rejected on the way *out* is a row already
//! written and unreadable, while one rejected on the way *in* is a block the
//! store is refusing to write.

use super::error_map::{corrupt_row, corrupt_row_because};
use zaino_chain_store::{
    ChainStoreError, ChainStoreReaderCapability, CompactBlockReadCapability, MigrationState,
    SchemaVersion, SpentOutputIndexCapability, StoreCapabilities, StoreCapability, StoreSchema,
    StoredAddress, StoredBlock, StoredBlockReadCapability, StoredTx, StoredTxOut,
    TransactionIndexCapability, TxOutSetIndexCapability,
};
use zaino_primitives::types::{
    BlockHash as DomainBlockHash, BlockHeader, BlockRef, BlockTxPosition,
    ChainWork as DomainChainWork, EncryptedCiphertext, Height as DomainHeight, Nullifier,
    OrchardAction, Outpoint as DomainOutpoint, PreIndexCompactTx, SaplingOutput, Script,
    ScriptType, SignedZatoshis, TransactionId, TransparentInput, TransparentOutput, TreeRootInfo,
    TreeRoots, TxIndex, Zatoshis,
};

use crate::store::capability::{Capability, DbMetadata, MigrationStatus};
use crate::store::finalised_source::v1::DB_VERSION_V1;
use crate::types::{
    BlockHash, CommitmentTreeData, CompactTxData, Height, IndexedBlock, Outpoint, TransactionHash,
    TransparentCompactTx, TxLocation, TxOutCompact,
};

/// This crate's height, as the domain names it.
///
/// The stored height is any `u32`; the domain's is validated against the
/// protocol maximum. A stored height that cannot be expressed is a corrupt row,
/// not a caller error, so it surfaces as [`ChainStoreError::CorruptRow`] rather
/// than being clamped into a height that names a different block.
pub(super) fn domain_height(height: Height) -> Result<DomainHeight, ChainStoreError> {
    DomainHeight::try_from(height.0).map_err(|error| {
        corrupt_row_because(format!("valid height for stored value {height}"), error)
    })
}

/// The same 32 bytes, as the domain names them.
pub(super) fn domain_hash(hash: BlockHash) -> DomainBlockHash {
    DomainBlockHash::from(hash.0)
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
pub(super) fn block_tx_position(location: TxLocation) -> Result<BlockTxPosition, ChainStoreError> {
    Ok(BlockTxPosition {
        height: domain_height(Height(location.block_height()))?,
        tx_index: TxIndex::from(location.tx_index()),
    })
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
pub(super) fn domain_txid(txid: TransactionHash) -> TransactionId {
    TransactionId::from(txid.0)
}

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
pub(super) fn stored_tx_outs(
    transparent: &TransparentCompactTx,
) -> Result<Vec<StoredTxOut>, ChainStoreError> {
    transparent.outputs().iter().map(stored_tx_out).collect()
}

/// A stored block, as the domain names it.
///
/// The header is reassembled from two stored pieces: the context, which carries
/// the identity an index reads (hash, parent, height), and the data, which
/// carries the consensus fields. They are separate on disk because they are
/// written to separate tables; nothing above this cares.
pub(super) fn stored_block(block: IndexedBlock) -> Result<StoredBlock, ChainStoreError> {
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
        sapling_value: stored_value_balance(sapling_value, "sapling")?,
        orchard_value: stored_value_balance(orchard_value, "orchard")?,
        ironwood_value: stored_value_balance(tx.ironwood().value(), "ironwood")?,
        compact: stored_compact_tx_body(tx)?,
    })
}

/// Read a stored per-pool value balance, or `None` where the pool is absent.
///
/// The on-disk `i64` is the boundary the delta's invariant is enforced at: a
/// value whose magnitude exceeds the money supply is not a representable balance
/// change, so it surfaces as a corrupt row rather than being carried into the
/// domain.
fn stored_value_balance(
    raw: Option<i64>,
    pool: &str,
) -> Result<Option<SignedZatoshis>, ChainStoreError> {
    raw.map(SignedZatoshis::try_new).transpose().map_err(|e| {
        corrupt_row_because(format!("a {pool} value balance within the money supply"), e)
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
pub(super) fn domain_chainwork(chainwork: &crate::types::ChainWork) -> DomainChainWork {
    let mut bytes = [0u8; 32];
    bytes[16..].copy_from_slice(&chainwork.as_non_zero_u128().get().to_be_bytes());
    DomainChainWork::new(bytes)
}

pub(super) fn tree_roots(data: &CommitmentTreeData) -> TreeRoots {
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
/// Every capability is named as `<R as SomePortCapability>::CAPABILITY` rather
/// than as a [`StoreCapability`] variant, so the advertisement is tied to the
/// port that answers it. Choosing variants by hand let the two drift in both
/// directions: a store could advertise an index it does not serve, or serve one
/// it never advertises, and both compile. Now a capability can only be
/// advertised for a port `R` actually implements — drop one of these impls and
/// this stops compiling instead of quietly over-promising.
///
/// The `CAPABILITY` comes from the sealed carrier trait, not the port, so the
/// pairing cannot be restated by an implementor even by accident; see
/// [`sealed_capability`](zaino_chain_store::ChainStoreReaderCapability).
///
/// It does not make the runtime bits agree with the compile-time impls; that
/// is what the bits are for, since this backend implements every port and
/// decides at runtime which it can serve. What it fixes is the mapping between
/// the two being restated here rather than read from the port.
pub(super) fn store_capabilities<R>(capability: Capability) -> StoreCapabilities
where
    R: ChainStoreReaderCapability
        + StoredBlockReadCapability
        + CompactBlockReadCapability
        + TransactionIndexCapability
        + SpentOutputIndexCapability
        + TxOutSetIndexCapability,
{
    let mut capabilities = vec![<R as ChainStoreReaderCapability>::CAPABILITY];

    // Stored blocks need the header, the txids and every per-pool surface: a
    // block missing one of them is not a block this store can hand over.
    if capability.has(
        Capability::CHAIN_BLOCK_EXT
            .union(Capability::BLOCK_CORE_EXT)
            .union(Capability::BLOCK_TRANSPARENT_EXT)
            .union(Capability::BLOCK_SHIELDED_EXT),
    ) {
        capabilities.push(<R as StoredBlockReadCapability>::CAPABILITY);
    }
    if capability.has(Capability::COMPACT_BLOCK_EXT) {
        capabilities.push(<R as CompactBlockReadCapability>::CAPABILITY);
    }
    if capability.has(Capability::BLOCK_CORE_EXT) {
        capabilities.push(<R as TransactionIndexCapability>::CAPABILITY);
    }
    // Both halves: the domain's spent-output surface answers "what did this
    // outpoint hold" from the transparent rows as well as "who spent it" from
    // the spent index, and a store with only one of them cannot serve it.
    if capability.has(Capability::SPENT_OUTPUT_INDEX.union(Capability::BLOCK_TRANSPARENT_EXT)) {
        capabilities.push(<R as SpentOutputIndexCapability>::CAPABILITY);
    }
    if capability.has(Capability::TXOUT_SET_INDEX) {
        capabilities.push(<R as TxOutSetIndexCapability>::CAPABILITY);
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
pub(super) fn store_schema(metadata: &DbMetadata) -> StoreSchema {
    let version = schema_version(metadata.version());
    let migration = match metadata.migration_status() {
        MigrationStatus::Empty | MigrationStatus::Complete => MigrationState::Settled,
        MigrationStatus::PartialBuildInProgress
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::reader::DbReader;
    use zaino_chain_store::{
        ChainStoreReaderCapability, CompactBlockReadCapability, SpentOutputIndexCapability,
        StoredBlockReadCapability, TransactionIndexCapability, TxOutSetIndexCapability,
    };

    /// The concrete reader whose ports the capability mapping is read from.
    type Reader = DbReader<zaino_source_zebra::ZebraValidator>;

    /// The capabilities a store on these bits advertises.
    fn advertised(capability: Capability) -> StoreCapabilities {
        store_capabilities::<Reader>(capability)
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
            <Reader as ChainStoreReaderCapability>::CAPABILITY,
            <Reader as StoredBlockReadCapability>::CAPABILITY,
            <Reader as CompactBlockReadCapability>::CAPABILITY,
            <Reader as TransactionIndexCapability>::CAPABILITY,
            <Reader as SpentOutputIndexCapability>::CAPABILITY,
            <Reader as TxOutSetIndexCapability>::CAPABILITY,
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
                        == <Reader as zaino_chain_store::TransparentHistoryIndexCapability>::CAPABILITY,
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
