//! Standalone, parallel-buildable finalised spend index: maps each transparent
//! outpoint to the txid of the transaction that consumed it.
//!
//! Proof of concept, gated behind the `outp_to_spend_index` feature. See
//! `docs/adr/0002-finalised-spend-index-parallel-build.md`.
//!
//! The build stages live here: **extract** spends from a block batch,
//! **collate** them into LMDB key order, and bulk-load them into the index's
//! own LMDB store via `MDB_APPEND` ([`SpendIndexDb`]). The independent sync
//! loop that drives these (a genesis re-stream from zebra) lands in a later
//! slice. (Table-level integrity over the entries is deferred; it is not MVP.)

use std::path::Path;

use lmdb::{Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction as _, WriteFlags};

use crate::chain_index::finalised_state::build_indexed_block_from_source;
use crate::chain_index::source::validator_connector::{StateSource, ValidatorConnector};
use crate::chain_index::source::BlockchainSource;
use crate::chain_index::types::TransactionHash;
use crate::chain_index::NON_FINALIZED_DEPTH;
use crate::error::FinalisedStateError;
use crate::{ChainWork, IndexedBlock, Outpoint, TransparentCompactTx, ZainoVersionedSerde as _};
use zaino_common::Network;
use zebra_chain::parameters::NetworkUpgrade;

/// One transparent spend: the consumed outpoint paired with the txid of the
/// transaction that consumed it.
pub(super) type SpendRecord = (Outpoint, TransactionHash);

/// A spend entry encoded for storage: the LMDB key (the encoded outpoint) and
/// value (the bare 32-byte spending txid), ready for an `MDB_APPEND` load.
pub(super) type EncodedSpend = (Vec<u8>, [u8; 32]);

// ── Extract ──────────────────────────────────────────────────────────────────

/// Extracts every transparent spend recorded in `blocks`.
///
/// Pure and statically read-free: it is handed only block data — no database
/// and no validator handle — so a previous-output lookup is unrepresentable,
/// not merely discouraged. Each transparent input yields
/// `(prevout_outpoint, spending_txid)` directly, where the spending txid is the
/// containing transaction's own id; nothing outside the block stream is
/// consulted.
///
/// TODO (loop slice): add a buffer-filling variant that writes into a
/// caller-owned `&mut Vec<SpendRecord>`, once the per-worker loop exists to
/// reuse one allocation across batches (`Vec::clear` keeps capacity ⇒ ~zero
/// steady-state alloc, and the collator can sort that buffer in place).
/// Deferred until a real reusing caller anchors it: the extractor is dwarfed by
/// zebra block I/O and the sort, so buffer reuse only pays then.
pub(super) fn extract_spends(blocks: &[IndexedBlock]) -> Vec<SpendRecord> {
    blocks
        .iter()
        .flat_map(|block| block.transactions().iter())
        .flat_map(|tx| spends_in_transaction(*tx.txid(), tx.transparent()))
        .collect()
}

/// The spends contributed by one transaction: each outpoint it consumes paired
/// with `spending_txid`. The non-coinbase filtering lives in
/// [`TransparentCompactTx::spent_outpoints`]; this only attaches the spender.
fn spends_in_transaction(
    spending_txid: TransactionHash,
    transparent: &TransparentCompactTx,
) -> impl Iterator<Item = SpendRecord> + '_ {
    transparent
        .spent_outpoints()
        .map(move |outpoint| (outpoint, spending_txid))
}

// ── Collate ──────────────────────────────────────────────────────────────────

/// Encodes and sorts `records` into LMDB key order — byte-wise on the encoded
/// outpoint key, matching LMDB's default comparator — so the result can be
/// bulk-loaded with `MDB_APPEND`.
///
/// Spend keys are globally disjoint — each outpoint is spent at most once on a
/// chain — so this is a pure sort needing no cross-record reconciliation; a
/// duplicate key means corrupt input and is rejected.
pub(super) fn collate(records: &[SpendRecord]) -> Result<Vec<EncodedSpend>, FinalisedStateError> {
    let mut encoded = records
        .iter()
        .map(|(outpoint, spending_txid)| {
            Ok((outpoint.to_bytes()?, <[u8; 32]>::from(*spending_txid)))
        })
        .collect::<Result<Vec<EncodedSpend>, FinalisedStateError>>()?;

    encoded.sort_by(|a, b| a.0.cmp(&b.0));

    if let Some(duplicate) = encoded.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(FinalisedStateError::Custom(format!(
            "duplicate spend-index key during collation: {:?}",
            duplicate[0].0
        )));
    }

    Ok(encoded)
}

// ── Store ────────────────────────────────────────────────────────────────────

/// LMDB database name for the spend index (versioned, mirroring the
/// finalised-state naming convention).
const SPEND_DB_NAME: &str = "outp_to_spend_index_1_0_0";

/// The spend index's **own** LMDB store: a single database keyed by the encoded
/// outpoint, valued by the bare 32-byte spending txid. Standalone — its own
/// `Environment`, independent of the finalised-state monolith's database.
pub(super) struct SpendIndexDb {
    env: Environment,
    spend: Database,
}

impl SpendIndexDb {
    /// Opens (creating if absent) the spend index at `path` with the given LMDB
    /// `map_size`. Mirrors the finalised-state environment: **no `WRITE_MAP`**
    /// (a crash discards uncommitted data rather than corrupting), with
    /// reader-friendly flags. `path` is a directory, created if missing.
    pub(super) fn open(path: &Path, map_size: usize) -> Result<Self, FinalisedStateError> {
        std::fs::create_dir_all(path).map_err(|error| {
            FinalisedStateError::Custom(format!(
                "spend index: create dir {}: {error}",
                path.display()
            ))
        })?;
        let env = Environment::new()
            .set_max_dbs(1)
            .set_map_size(map_size)
            .set_flags(EnvironmentFlags::NO_TLS | EnvironmentFlags::NO_READAHEAD)
            .open(path)?;
        let spend = env.create_db(Some(SPEND_DB_NAME), DatabaseFlags::empty())?;
        Ok(Self { env, spend })
    }

    /// Bulk-loads collated (sorted, disjoint-key) entries with `MDB_APPEND` — a
    /// sequential B-tree fill — in one transaction, then forces durability.
    /// `entries` must be in ascending key order (see [`collate`]); a key out of
    /// order or already present makes LMDB reject the append.
    pub(super) fn bulk_load(&self, entries: &[EncodedSpend]) -> Result<(), FinalisedStateError> {
        // One-shot / global-order contract: `entries` must be strictly ascending
        // by key — `collate`'s output for *all* spends at once. `MDB_APPEND`
        // requires every key to exceed the current maximum, so there is no
        // re-sort across calls: a second `bulk_load` would need every key to
        // exceed the first call's maximum. LMDB self-checks this at runtime (it
        // rejects a non-increasing key with `KeyExist`); this assert fails earlier
        // and with a clearer message in debug builds if a caller hands over an
        // unsorted, duplicate, or per-batch (not globally sorted) input.
        debug_assert!(
            entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "bulk_load: entries must be strictly ascending, globally-sorted keys \
             (MDB_APPEND is one-shot — feed all spends through a single collate)",
        );
        let mut txn = self.env.begin_rw_txn()?;
        for (key, value) in entries {
            txn.put(self.spend, key, value, WriteFlags::APPEND)?;
        }
        txn.commit()?;
        self.env.sync(true)?;
        Ok(())
    }

    /// The txid that consumed `outpoint`, or `None` if it is unspent in
    /// finalised state. A single point lookup; the caller unions this with the
    /// non-finalised window at serve time (the deferred seam union).
    pub(super) fn spending_txid(
        &self,
        outpoint: &Outpoint,
    ) -> Result<Option<TransactionHash>, FinalisedStateError> {
        let key = outpoint.to_bytes()?;
        let ro = self.env.begin_ro_txn()?;
        match ro.get(self.spend, &key) {
            Ok(bytes) => {
                let array: [u8; 32] = bytes.try_into().map_err(|_| {
                    FinalisedStateError::Custom(
                        "spend index: stored value is not a 32-byte txid".to_string(),
                    )
                })?;
                Ok(Some(TransactionHash::from(array)))
            }
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

// ── Sync loop ────────────────────────────────────────────────────────────────

/// The sources the spend-index POC may build from: zebra's StateService
/// ([`StateSource`]) and the test mockchain — **never** the JSON-RPC/zcashd
/// `FetchService`. Binding [`SpendIndexSync`] to `S: SpendIndexSource` makes
/// "never FetchService" a compile-time fact: the `Fetch` path does not
/// implement this trait.
pub(super) trait SpendIndexSource: BlockchainSource {}

impl SpendIndexSource for StateSource {}

#[cfg(test)]
impl SpendIndexSource for crate::chain_index::source::mockchain_source::MockchainSource {}

/// The independent, single-run builder for the spend index.
///
/// Move-only (`!Clone`) with a private constructor and a `self`-consuming
/// [`run`](Self::run): the type system makes a second concurrent build
/// unrepresentable — "only one loop at a time" without a runtime guard. POC
/// build model: re-stream `start_height` → finalised tip from `source`, extract
/// every spend, collate once, and bulk-load the index in a single `MDB_APPEND`
/// pass.
pub(super) struct SpendIndexSync<S: SpendIndexSource> {
    source: S,
    db: SpendIndexDb,
    network: Network,
    /// First height to index, inclusive. Genesis (`0`) for a full index, or a
    /// higher height — e.g. a network-upgrade activation — to cover only spends
    /// occurring from that epoch onward.
    start_height: u32,
}

impl<S: SpendIndexSource> SpendIndexSync<S> {
    /// Private mint: production enters via [`SpendIndexSync::from_state`]
    /// (StateService only); tests construct directly with a `MockchainSource`.
    fn new(source: S, db: SpendIndexDb, network: Network, start_height: u32) -> Self {
        Self {
            source,
            db,
            network,
            start_height,
        }
    }

    /// Builds the spend index from its start height to the finalised tip, consuming the
    /// handle. One-shot (POC): no resume, no batching — a single global
    /// `collate` + `MDB_APPEND`.
    pub(super) async fn run(self) -> Result<(), FinalisedStateError> {
        let Some(best) = self.source.get_best_block_height().await.map_err(|error| {
            FinalisedStateError::Custom(format!("spend index: fetch best height: {error:?}"))
        })?
        else {
            return Ok(()); // empty chain — nothing finalised to index
        };

        // Finalised tip: below the reorg-possible window, clamped at genesis.
        let finalised_tip = best.0.saturating_sub(NON_FINALIZED_DEPTH);

        let zebra_network = self.network.to_zebra_network();
        let sapling_activation = NetworkUpgrade::Sapling
            .activation_height(&zebra_network)
            .expect("Sapling activation height is set for every network");
        let nu5_activation = NetworkUpgrade::Nu5.activation_height(&zebra_network);

        // Stream finalised blocks, extracting spends and dropping each block;
        // only the (smaller) spend records are retained for the one-shot sort.
        // Chainwork is irrelevant to spend extraction, so a zero parent is fine.
        let mut spends: Vec<SpendRecord> = Vec::new();
        for height in self.start_height..=finalised_tip {
            let block = build_indexed_block_from_source(
                &self.source,
                self.network,
                sapling_activation,
                nu5_activation,
                height,
                ChainWork::from_u256(0.into()),
            )
            .await?;
            spends.extend(extract_spends(std::slice::from_ref(&block)));
        }

        let collated = collate(&spends)?;
        tokio::task::block_in_place(|| self.db.bulk_load(&collated))?;
        Ok(())
    }
}

impl SpendIndexSync<StateSource> {
    /// Production constructor — **StateService only**. The `Fetch`
    /// (JSON-RPC/zcashd) backend is rejected here, the one place the concrete
    /// `ValidatorConnector` variant is visible; the `S: SpendIndexSource` bound
    /// already keeps a `Fetch` source from compiling elsewhere.
    pub(super) fn from_state(
        connector: ValidatorConnector,
        db: SpendIndexDb,
        network: Network,
        start_height: u32,
    ) -> Result<Self, FinalisedStateError> {
        match connector {
            ValidatorConnector::State(state) => {
                Ok(Self::new(StateSource(state), db, network, start_height))
            }
            ValidatorConnector::Fetch(_) => Err(FinalisedStateError::Custom(
                "spend-index POC requires the StateService backend, not FetchService".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod spends_in_transaction {
    use super::*;
    use crate::TxInCompact;

    /// Arbitrary fill byte for the spending transaction's txid, kept distinct
    /// from the prevout txids below so the two can't be confused.
    const SPENDER_TXID_BYTE: u8 = 9;

    fn txid(byte: u8) -> TransactionHash {
        TransactionHash::from([byte; 32])
    }

    #[test]
    fn skips_coinbase_input_keeps_real_spends() {
        let spender = txid(SPENDER_TXID_BYTE);
        let transparent = TransparentCompactTx::new(
            vec![
                TxInCompact::null_prevout(),    // coinbase input → contributes nothing
                TxInCompact::new([1u8; 32], 0), // spends output 0 of txid 0x01..
                TxInCompact::new([2u8; 32], 7), // spends output 7 of txid 0x02..
            ],
            vec![],
        );

        let records: Vec<SpendRecord> =
            super::spends_in_transaction(spender, &transparent).collect();

        assert_eq!(
            records,
            vec![
                (Outpoint::new([1u8; 32], 0), spender),
                (Outpoint::new([2u8; 32], 7), spender),
            ]
        );
    }
}

#[cfg(test)]
mod collate {
    use super::*;

    fn record(outpoint_byte: u8, index: u32) -> SpendRecord {
        (
            Outpoint::new([outpoint_byte; 32], index),
            TransactionHash::from([0xaau8; 32]),
        )
    }

    #[test]
    fn sorts_into_ascending_key_order() {
        let records = [record(3, 0), record(1, 0), record(2, 0)];
        let encoded = super::collate(&records).expect("disjoint keys collate cleanly");
        assert!(
            encoded.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "collated keys must be strictly ascending for MDB_APPEND",
        );
    }

    #[test]
    fn rejects_duplicate_keys() {
        let records = [record(1, 0), record(1, 0)];
        assert!(super::collate(&records).is_err());
    }

    #[test]
    fn little_endian_index_tie_break_matches_byte_order() {
        // Same creating txid, two output indices. The index is LE-encoded, so
        // 256 (bytes `00 01 00 00`) sorts before 1 (`01 00 00 00`) under memcmp
        // — exactly the order LMDB's default comparator (and thus MDB_APPEND)
        // requires. collate must reproduce that, not a numeric ordering.
        let encoded =
            super::collate(&[record(7, 1), record(7, 256)]).expect("disjoint keys collate cleanly");

        assert!(
            encoded[0].0 < encoded[1].0,
            "keys must be strictly ascending for MDB_APPEND",
        );
        // The trailing LE u32 of each key shows index 256 precedes index 1.
        assert_eq!(
            &encoded[0].0[encoded[0].0.len() - 4..],
            &256u32.to_le_bytes()[..]
        );
        assert_eq!(
            &encoded[1].0[encoded[1].0.len() - 4..],
            &1u32.to_le_bytes()[..]
        );
    }
}

#[cfg(test)]
mod spend_index_db {
    use super::*;

    #[test]
    fn bulk_load_then_point_read_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "outp_to_spend_index_roundtrip_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let db = SpendIndexDb::open(&dir, 1 << 20).expect("open spend index");

        let spent_a = Outpoint::new([1u8; 32], 0);
        let spent_b = Outpoint::new([2u8; 32], 7);
        let spender = TransactionHash::from([9u8; 32]);

        let collated = super::collate(&[(spent_a, spender), (spent_b, spender)]).expect("collate");
        db.bulk_load(&collated).expect("bulk load");

        assert_eq!(db.spending_txid(&spent_a).expect("read a"), Some(spender));
        assert_eq!(db.spending_txid(&spent_b).expect("read b"), Some(spender));
        // An outpoint with no recorded spend reads back as unspent.
        let unspent = Outpoint::new([3u8; 32], 0);
        assert_eq!(db.spending_txid(&unspent).expect("read unspent"), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
