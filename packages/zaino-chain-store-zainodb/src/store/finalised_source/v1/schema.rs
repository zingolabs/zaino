//! The v1 database's tables, and the schema hash this build computes from what it actually writes.
//!
//! The hash covers the encoding of one canonical instance of every stored record, every table's
//! name and creation flags, every singleton key, and the enabled index features. Any change to how
//! this build lays data out therefore changes the hash, and a database carrying a different hash is
//! rebuilt on open. The goldens in `golden.rs` pin the same canonical encodings, so a layout change
//! also fails a test that names the record, which is where a reviewer accepts the rebuild cost.

use blake2::{
    digest::{Update, VariableOutput},
    Blake2bVar,
};
use corez::io;
use lmdb::DatabaseFlags;

use crate::codec::{CompactSize, DbCodec};
use crate::error::StoreError;

/// An LMDB table in the v1 environment, with the flags it is created with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Table {
    /// The LMDB database name.
    pub(crate) name: &'static str,
    /// The flags the table is created with.
    pub(crate) flags: DatabaseFlags,
}

/// Block headers keyed by height.
pub(crate) const HEADERS: Table = plain_table("headers");
/// Block txid lists keyed by height.
pub(crate) const TXIDS: Table = plain_table("txids");
/// Per-transaction transparent data keyed by height.
pub(crate) const TRANSPARENT: Table = plain_table("transparent");
/// Per-transaction Sapling data keyed by height.
pub(crate) const SAPLING: Table = plain_table("sapling");
/// Per-transaction Orchard data keyed by height.
pub(crate) const ORCHARD: Table = plain_table("orchard");
/// Per-transaction Ironwood data keyed by height, present only for blocks with Ironwood data.
pub(crate) const IRONWOOD: Table = plain_table("ironwood");
/// Commitment tree roots and sizes keyed by height.
pub(crate) const COMMITMENT_TREE_DATA: Table = plain_table("commitment_tree_data");
/// Block heights keyed by block hash.
pub(crate) const HEIGHTS: Table = plain_table("heights");
/// Spending transaction locations keyed by outpoint.
pub(crate) const SPENT: Table = plain_table("spent");
/// Transaction locations keyed by txid.
pub(crate) const TXID_LOCATION: Table = plain_table("txid_location");
/// The txout-set accumulator singleton.
pub(crate) const TX_OUT_SET_INFO_ACCUMULATOR: Table = plain_table("tx_out_set_info_accumulator");
/// The metadata singleton and the accumulator's built-height watermark.
pub(crate) const METADATA: Table = plain_table("metadata");
/// Address history events, as fixed-width duplicate values keyed by address script.
#[cfg(feature = "transparent_address_history_experimental")]
pub(crate) const ADDRESS_HISTORY: Table = Table {
    name: "address_history",
    flags: DatabaseFlags::DUP_SORT.union(DatabaseFlags::DUP_FIXED),
};

/// Every table this build creates.
const TABLES: &[Table] = &[
    HEADERS,
    TXIDS,
    TRANSPARENT,
    SAPLING,
    ORCHARD,
    IRONWOOD,
    COMMITMENT_TREE_DATA,
    HEIGHTS,
    SPENT,
    TXID_LOCATION,
    TX_OUT_SET_INFO_ACCUMULATOR,
    METADATA,
    #[cfg(feature = "transparent_address_history_experimental")]
    ADDRESS_HISTORY,
];

/// Every singleton key this build writes into a table.
const SINGLETON_KEYS: &[&[u8]] = &[
    super::METADATA_KEY,
    super::TX_OUT_SET_INFO_ACCUMULATOR_KEY,
    super::TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY,
];

/// Every optional index compiled into this build.
const ENABLED_INDEX_FEATURES: &[&str] = &[
    #[cfg(feature = "transparent_address_history_experimental")]
    "transparent_address_history_experimental",
];

/// A table created with no flags.
const fn plain_table(name: &'static str) -> Table {
    Table {
        name,
        flags: DatabaseFlags::empty(),
    }
}

impl Table {
    /// Opens this table in `env`, creating it with its flags when it does not exist yet.
    pub(crate) async fn open(self, env: &lmdb::Environment) -> Result<lmdb::Database, StoreError> {
        super::super::open_or_create_db(env, self.name, self.flags).await
    }
}

/// Computes this build's schema hash from its record encodings, tables, singleton keys and index features.
pub(crate) fn schema_hash() -> io::Result<[u8; 32]> {
    Ok(hash_schema(
        &canonical_encodings()?,
        TABLES,
        SINGLETON_KEYS,
        ENABLED_INDEX_FEATURES,
    ))
}

/// Returns each stored record type's name with the encoding of its canonical instance.
pub(crate) fn canonical_encodings() -> io::Result<Vec<(&'static str, Vec<u8>)>> {
    #[cfg_attr(
        not(feature = "transparent_address_history_experimental"),
        allow(unused_mut)
    )]
    let mut encodings = vec![
        ("BlockHash", canonical::block_hash().to_bytes()?),
        ("Height", canonical::height().to_bytes()?),
        (
            "BlockHeaderData",
            canonical::block_header_data().to_bytes()?,
        ),
        (
            "EquihashSolution::Standard",
            canonical::standard_equihash_solution().to_bytes()?,
        ),
        ("TxidList", canonical::txid_list().to_bytes()?),
        (
            "TransparentTxList",
            canonical::transparent_tx_list().to_bytes()?,
        ),
        ("SaplingTxList", canonical::sapling_tx_list().to_bytes()?),
        ("OrchardTxList", canonical::orchard_tx_list().to_bytes()?),
        (
            "CommitmentTreeData",
            canonical::commitment_tree_data().to_bytes()?,
        ),
        ("TransactionHash", canonical::transaction_hash().to_bytes()?),
        ("Outpoint", canonical::outpoint().to_bytes()?),
        ("TxLocation", canonical::tx_location().to_bytes()?),
        (
            "FinalisedTxOutSetInfoAccumulator",
            canonical::txout_set_accumulator().to_bytes()?,
        ),
        ("DbMetadata", canonical::db_metadata().to_bytes()?),
    ];
    #[cfg(feature = "transparent_address_history_experimental")]
    encodings.extend([
        ("AddrScript", canonical::addr_script().to_bytes()?),
        ("AddrEventBytes", canonical::addr_event_bytes()?.to_bytes()?),
    ]);
    Ok(encodings)
}

/// Hashes the schema inputs with BLAKE2b-256, length-prefixing every item so no two schemas share an input stream.
fn hash_schema(
    encodings: &[(&str, Vec<u8>)],
    tables: &[Table],
    singleton_keys: &[&[u8]],
    features: &[&str],
) -> [u8; 32] {
    let mut input = Vec::new();
    let mut push = |bytes: &[u8]| {
        CompactSize::write(&mut input, bytes.len()).expect("writing to a Vec cannot fail");
        input.extend_from_slice(bytes);
    };

    push(b"zaino finalised store schema");
    for (name, encoding) in encodings {
        push(name.as_bytes());
        push(encoding);
    }
    for table in tables {
        push(table.name.as_bytes());
        push(&table.flags.bits().to_le_bytes());
    }
    for key in singleton_keys {
        push(key);
    }
    for feature in features {
        push(feature.as_bytes());
    }

    let mut hasher = Blake2bVar::new(32).expect("32 is a valid BLAKE2b output length");
    hasher.update(&input);
    let mut hash = [0u8; 32];
    hasher
        .finalize_variable(&mut hash)
        .expect("the output buffer matches the requested length");
    hash
}

/// One canonical instance of every record type the store encodes, each field a distinct repeated byte.
pub(crate) mod canonical {
    use core::num::NonZeroU128;

    use corez::io;

    use crate::store::capability::DbMetadata;
    use crate::types::db::commitment::{
        CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes,
    };
    use crate::types::db::legacy::AddrEventBytes;
    use crate::types::db::metadata::FinalisedTxOutSetInfoAccumulator;
    use crate::types::{
        AbsoluteChainWork, AddrHistRecord, AddrScript, BlockContext, BlockData, BlockHash,
        BlockHeaderData, CompactDifficulty, CompactOrchardAction, CompactSaplingOutput,
        CompactSaplingSpend, EquihashSolution, Height, OrchardCompactTx, OrchardTxList, Outpoint,
        SaplingCompactTx, SaplingTxList, ScriptType, TransactionHash, TransparentCompactTx,
        TransparentTxList, TxInCompact, TxLocation, TxOutCompact, TxidList,
    };

    /// The chainwork the canonical block context carries.
    const CHAINWORK: NonZeroU128 = match NonZeroU128::new(0x0dec_0de0) {
        Some(chainwork) => chainwork,
        None => panic!("the canonical chainwork literal is nonzero"),
    };

    /// A valid nBits value that passes zebra's compact-difficulty validation without corresponding to any real block.
    const VALID_NBITS: u32 = 0x2007_ffff;

    /// The canonical block hash.
    pub(crate) fn block_hash() -> BlockHash {
        BlockHash::from([0x11; 32])
    }

    /// The canonical transaction hash.
    pub(crate) fn transaction_hash() -> TransactionHash {
        TransactionHash::from([0x22; 32])
    }

    /// The canonical height.
    pub(crate) fn height() -> Height {
        Height(123_456)
    }

    /// The canonical address script.
    pub(crate) fn addr_script() -> AddrScript {
        AddrScript::new([0x33; 20], ScriptType::P2SH as u8)
    }

    /// The canonical outpoint.
    pub(crate) fn outpoint() -> Outpoint {
        Outpoint::new([0x44; 32], 7)
    }

    /// The canonical Equihash solution, the short regtest variant so the goldens that embed it stay readable.
    pub(crate) fn equihash_solution() -> EquihashSolution {
        EquihashSolution::Regtest([0x55; 36])
    }

    /// The canonical mainnet-length Equihash solution, which pins the other variant's layout.
    pub(crate) fn standard_equihash_solution() -> EquihashSolution {
        EquihashSolution::Standard([0x56; 1344])
    }

    /// The canonical block data.
    pub(crate) fn block_data() -> BlockData {
        BlockData {
            version: 4,
            time: 0x6543_2100,
            merkle_root: [0x66; 32],
            block_commitments: [0x77; 32],
            bits: CompactDifficulty::try_from_bits(VALID_NBITS)
                .expect("the canonical nBits literal is valid"),
            nonce: [0x88; 32],
            solution: equihash_solution(),
        }
    }

    /// The canonical block context.
    pub(crate) fn block_context() -> BlockContext<AbsoluteChainWork> {
        BlockContext::new(
            block_hash(),
            BlockHash::from([0x99; 32]),
            AbsoluteChainWork::new(CHAINWORK),
            height(),
        )
    }

    /// The canonical block header.
    pub(crate) fn block_header_data() -> BlockHeaderData<AbsoluteChainWork> {
        BlockHeaderData::new(block_context(), block_data())
    }

    /// The canonical transparent input.
    pub(crate) fn tx_in_compact() -> TxInCompact {
        TxInCompact::new([0xaa; 32], 3)
    }

    /// The canonical transparent output.
    pub(crate) fn tx_out_compact() -> TxOutCompact {
        TxOutCompact::new(21_000_000, [0xbb; 20], ScriptType::P2PKH as u8)
            .expect("the canonical value is within the compact range")
    }

    /// The canonical transparent transaction.
    pub(crate) fn transparent_compact_tx() -> TransparentCompactTx {
        TransparentCompactTx::new(vec![tx_in_compact()], vec![tx_out_compact()])
    }

    /// The canonical Sapling spend.
    pub(crate) fn compact_sapling_spend() -> CompactSaplingSpend {
        CompactSaplingSpend::new([0xcc; 32])
    }

    /// The canonical Sapling output.
    pub(crate) fn compact_sapling_output() -> CompactSaplingOutput {
        CompactSaplingOutput::new([0xdd; 32], [0xde; 32], [0xdf; 52])
    }

    /// The canonical Sapling transaction, with a negative value balance so the sign is pinned.
    pub(crate) fn sapling_compact_tx() -> SaplingCompactTx {
        SaplingCompactTx::new(
            Some(-4_200),
            vec![compact_sapling_spend()],
            vec![compact_sapling_output()],
        )
    }

    /// The canonical Orchard action.
    pub(crate) fn compact_orchard_action() -> CompactOrchardAction {
        CompactOrchardAction::new([0xe0; 32], [0xe1; 32], [0xe2; 32], [0xe3; 52])
    }

    /// The canonical Orchard transaction.
    pub(crate) fn orchard_compact_tx() -> OrchardCompactTx {
        OrchardCompactTx::new(Some(9_100), vec![compact_orchard_action()])
    }

    /// The canonical transaction location.
    pub(crate) fn tx_location() -> TxLocation {
        TxLocation::new(123_456, 9)
    }

    /// The canonical address history record.
    pub(crate) fn addr_hist_record() -> AddrHistRecord {
        AddrHistRecord::new(tx_location(), 2, 555_000, AddrEventBytes::FLAG_MINED)
    }

    /// The canonical packed address event, the stored form of the canonical address history record.
    pub(crate) fn addr_event_bytes() -> io::Result<AddrEventBytes> {
        AddrEventBytes::from_record(&addr_hist_record())
    }

    /// The canonical txid list.
    pub(crate) fn txid_list() -> TxidList {
        TxidList::new(vec![transaction_hash(), TransactionHash::from([0x23; 32])])
    }

    /// The canonical transparent list, with a `None` slot so the encoding of absence is pinned.
    pub(crate) fn transparent_tx_list() -> TransparentTxList {
        TransparentTxList::new(vec![Some(transparent_compact_tx()), None])
    }

    /// The canonical Sapling list, with a `None` slot.
    pub(crate) fn sapling_tx_list() -> SaplingTxList {
        SaplingTxList::new(vec![Some(sapling_compact_tx()), None])
    }

    /// The canonical Orchard list, with a `None` slot.
    pub(crate) fn orchard_tx_list() -> OrchardTxList {
        OrchardTxList::new(vec![Some(orchard_compact_tx()), None])
    }

    /// The canonical commitment tree roots, with the optional Ironwood root present so its layout is pinned.
    pub(crate) fn commitment_tree_roots() -> CommitmentTreeRoots {
        CommitmentTreeRoots::new([0x01; 32], [0x02; 32], Some([0x03; 32]))
    }

    /// The canonical commitment tree sizes.
    pub(crate) fn commitment_tree_sizes() -> CommitmentTreeSizes {
        CommitmentTreeSizes::new(11, 22, 33)
    }

    /// The canonical commitment tree data.
    pub(crate) fn commitment_tree_data() -> CommitmentTreeData {
        CommitmentTreeData::new(commitment_tree_roots(), commitment_tree_sizes())
    }

    /// The canonical txout-set accumulator.
    pub(crate) fn txout_set_accumulator() -> FinalisedTxOutSetInfoAccumulator {
        FinalisedTxOutSetInfoAccumulator::new(101, 202, 303, [0x5a; 32], 404)
    }

    /// The canonical metadata record, whose hash is a placeholder rather than any real schema hash.
    pub(crate) fn db_metadata() -> DbMetadata {
        DbMetadata::new([0x7e; 32])
    }
}

#[cfg(test)]
mod hash_schema {
    use super::*;

    fn tables() -> [Table; 2] {
        [plain_table("a"), plain_table("b")]
    }

    fn baseline() -> [u8; 32] {
        hash_schema(&[("Record", vec![1, 2, 3])], &tables(), &[b"key"], &[])
    }

    #[test]
    fn a_record_encoding_change_changes_the_hash() {
        assert_ne!(
            baseline(),
            hash_schema(&[("Record", vec![1, 2, 4])], &tables(), &[b"key"], &[])
        );
    }

    #[test]
    fn a_table_flag_change_changes_the_hash() {
        let dup_sorted = [
            plain_table("a"),
            Table {
                name: "b",
                flags: DatabaseFlags::DUP_SORT,
            },
        ];
        assert_ne!(
            baseline(),
            hash_schema(&[("Record", vec![1, 2, 3])], &dup_sorted, &[b"key"], &[])
        );
    }

    #[test]
    fn enabling_an_index_feature_changes_the_hash() {
        assert_ne!(
            baseline(),
            hash_schema(
                &[("Record", vec![1, 2, 3])],
                &tables(),
                &[b"key"],
                &["index"]
            )
        );
    }

    #[test]
    fn moving_bytes_between_adjacent_items_changes_the_hash() {
        // Without length prefixes, ("ab", "c") and ("a", "bc") would hash the same stream.
        assert_ne!(
            hash_schema(&[("ab", b"c".to_vec())], &[], &[], &[]),
            hash_schema(&[("a", b"bc".to_vec())], &[], &[], &[])
        );
    }

    #[test]
    fn this_build_computes_a_schema_hash() {
        schema_hash().expect("every canonical record encodes");
    }

    /// The hash covers what the store writes into a layout, not the layout alone, so the record layouts, tables, keys and features by themselves must not reproduce it.
    #[test]
    fn the_hash_covers_more_than_the_record_layouts() {
        let layout_only = hash_schema(
            &canonical_encodings().expect("every canonical record encodes"),
            TABLES,
            SINGLETON_KEYS,
            ENABLED_INDEX_FEATURES,
        );
        assert_ne!(
            schema_hash().expect("every canonical record encodes"),
            layout_only,
            "a rule change that leaves every layout intact would not rebuild the database"
        );
    }
}
