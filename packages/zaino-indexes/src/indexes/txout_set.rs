//! TxOutSetIndex (CrossIndex × Append): height → the block's change to the
//! transparent UTXO-set summary.
//!
//! `gettxoutsetinfo` reports five facts about the set of unspent transparent
//! outputs: how many there are, how many transactions still hold one, their
//! total value, a serialized size, and a commitment to the set. This index
//! records what each block adds to and removes from those, so the summary at a
//! height is the fold of every entry up to it:
//!
//! ```text
//! summary(h) = Σ created(0..=h) − Σ spent(0..=h)        (counts and value)
//! hash(h)    = ⊕ digest(o)  over every o created or spent in 0..=h
//! ```
//!
//! The commitment is an XOR multiset: an output's digest is XORed in when it is
//! created and again when it is spent, so the two cancel and the fold is the XOR
//! over exactly the unspent outputs, whatever order blocks are folded in.
//!
//! # Why a cross index
//!
//! A transparent input names the outpoint it spends and nothing else. Removing
//! an output from the summary needs its value and script, so every spend is
//! resolved against where the output was created: `txid_location` finds the
//! creating transaction and `transparent_data` holds its outputs. Deciding
//! whether a spend empties its transaction reads `transparent_spends` for the
//! sibling outputs. All three are read through the [`DepsReader`], which also
//! sees what they have persisted for the batch in flight.
//!
//! # The commitment is a contract
//!
//! [`DOMAIN_TAG`], the field order and widths of [`canonical_entry`], and
//! [`ENTRY_LEN`] decide numbers a user can compare between two deployments.
//! They match the scheme the legacy finalised store commits to, so a store built
//! by either reports the same summary for the same chain. It is Zaino's own
//! commitment, not zcashd's `hash_serialized`.
//!
//! Only outputs whose script parses as P2PKH or P2SH are in the set, mirroring
//! zcashd's `IsUnspendable`; every other output contributes to no field.

use std::collections::HashMap;

use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{DecodeError, EntryCodec, KeyOrder, PersistentRecord};
use zaino_primitives::types::{
    classify_script, OutputIndex, Script, ScriptType, TransactionId, Zatoshis,
};
use zaino_sync::descriptor::{Append, CrossIndex};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{DepsReadError, DepsReader, ExtractCross, IndexDef, MergeAppend, Schema};

use crate::indexes::transparent_data::{
    self, TransparentBlockValue, TransparentDataIndex, TransparentTxCompact,
};
use crate::indexes::transparent_spends::{self, OutpointKey, TransparentSpendsIndex};
use crate::indexes::txid_location::{self, TxLocation, TxidLocationIndex};

/// Domain separator for the commitment, prepended to every entry before
/// hashing.
pub const DOMAIN_TAG: &[u8; 16] = b"ZcashTxOutSet___";

/// Canonical encoded length of one UTXO entry, in bytes:
/// `txid(32) + output_index(4) + value(8) + address_hash(20) + script_tag(1)`.
///
/// Not a storage size: `bytes_serialized` is this multiplied by the number of
/// unspent outputs.
pub const ENTRY_LEN: u64 = 32 + 4 + 8 + 20 + 1;

/// The commitment's tag for a script form, or `None` for a form that is not in
/// the set.
const fn script_tag(script_type: ScriptType) -> Option<u8> {
    match script_type {
        ScriptType::P2PKH => Some(0x00),
        ScriptType::P2SH => Some(0x01),
        ScriptType::NonStandard => None,
    }
}

/// The canonical bytes committed to for one unspent output.
pub fn canonical_entry(
    txid: TransactionId,
    output_index: OutputIndex,
    value: Zatoshis,
    address_hash: [u8; 20],
    script_tag: u8,
) -> [u8; 65] {
    let mut entry = [0u8; 65];
    entry[..32].copy_from_slice(&<[u8; 32]>::from(txid));
    entry[32..36].copy_from_slice(&output_index.to_le_bytes());
    entry[36..44].copy_from_slice(&value.as_u64().to_le_bytes());
    entry[44..64].copy_from_slice(&address_hash);
    entry[64] = script_tag;
    entry
}

/// `BLAKE2b-256(DOMAIN_TAG || canonical_entry)`.
fn entry_digest(entry: &[u8; 65]) -> [u8; 32] {
    let hash = blake2b_simd::Params::new()
        .hash_length(32)
        .to_state()
        .update(DOMAIN_TAG)
        .update(entry)
        .finalize();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(hash.as_bytes());
    digest
}

/// One output that is in the set: what its digest and its value are.
struct SetMember {
    digest: [u8; 32],
    value: Zatoshis,
}

/// The set member an output is, or `None` when its script keeps it out of the
/// set.
fn set_member(
    txid: TransactionId,
    output_index: OutputIndex,
    value: Zatoshis,
    script: &Script,
) -> Option<SetMember> {
    let (address_hash, script_type) = classify_script(script.as_bytes());
    let tag = script_tag(script_type)?;
    Some(SetMember {
        digest: entry_digest(&canonical_entry(
            txid,
            output_index,
            value,
            address_hash,
            tag,
        )),
        value,
    })
}

/// What a range of blocks adds to and removes from the UTXO-set summary.
///
/// Additions and removals are kept apart as unsigned running sums, so folding
/// never needs a signed intermediate; the summary subtracts once, at the end.
/// A commutative monoid under [`combine`](Self::combine), with
/// [`EMPTY`](Self::EMPTY) as identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TxOutSetDelta {
    /// Transactions that gained their first unspent output.
    pub transactions_added: u64,
    /// Transactions whose last unspent output was spent.
    pub transactions_removed: u64,
    /// Outputs created.
    pub outputs_added: u64,
    /// Outputs spent.
    pub outputs_removed: u64,
    /// Value created, in zatoshis.
    pub zatoshis_added: u64,
    /// Value spent, in zatoshis.
    pub zatoshis_removed: u64,
    /// XOR of the digest of every output created or spent.
    pub digest: [u8; 32],
}

/// A running sum left the `u64` range, or more was removed than added — a
/// corrupt or inconsistent store, never a real chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("UTXO-set summary arithmetic left its range in `{field}`")]
pub struct TxOutSetArithmeticError {
    /// The field whose arithmetic failed.
    pub field: &'static str,
}

fn add(left: u64, right: u64, field: &'static str) -> Result<u64, TxOutSetArithmeticError> {
    left.checked_add(right)
        .ok_or(TxOutSetArithmeticError { field })
}

fn net(added: u64, removed: u64, field: &'static str) -> Result<u64, TxOutSetArithmeticError> {
    added
        .checked_sub(removed)
        .ok_or(TxOutSetArithmeticError { field })
}

/// The UTXO-set summary a fold of deltas amounts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxOutSetTotals {
    /// Transactions with at least one unspent output.
    pub transactions: u64,
    /// Unspent outputs.
    pub tx_outs: u64,
    /// [`ENTRY_LEN`] times the number of unspent outputs.
    pub bytes_serialized: u64,
    /// The XOR multiset commitment over the unspent outputs.
    pub hash_serialized: [u8; 32],
    /// Total unspent value, in zatoshis.
    pub total_zatoshis: u64,
}

impl TxOutSetDelta {
    /// The delta of no blocks.
    pub const EMPTY: Self = Self {
        transactions_added: 0,
        transactions_removed: 0,
        outputs_added: 0,
        outputs_removed: 0,
        zatoshis_added: 0,
        zatoshis_removed: 0,
        digest: [0; 32],
    };

    /// The delta of both ranges together.
    pub fn combine(self, other: Self) -> Result<Self, TxOutSetArithmeticError> {
        let mut digest = self.digest;
        for (byte, other_byte) in digest.iter_mut().zip(other.digest) {
            *byte ^= other_byte;
        }
        Ok(Self {
            transactions_added: add(
                self.transactions_added,
                other.transactions_added,
                "transactions_added",
            )?,
            transactions_removed: add(
                self.transactions_removed,
                other.transactions_removed,
                "transactions_removed",
            )?,
            outputs_added: add(self.outputs_added, other.outputs_added, "outputs_added")?,
            outputs_removed: add(
                self.outputs_removed,
                other.outputs_removed,
                "outputs_removed",
            )?,
            zatoshis_added: add(self.zatoshis_added, other.zatoshis_added, "zatoshis_added")?,
            zatoshis_removed: add(
                self.zatoshis_removed,
                other.zatoshis_removed,
                "zatoshis_removed",
            )?,
            digest,
        })
    }

    /// The summary this delta amounts to when it covers the chain from genesis.
    pub fn totals(self) -> Result<TxOutSetTotals, TxOutSetArithmeticError> {
        let tx_outs = net(self.outputs_added, self.outputs_removed, "tx_outs")?;
        Ok(TxOutSetTotals {
            transactions: net(
                self.transactions_added,
                self.transactions_removed,
                "transactions",
            )?,
            tx_outs,
            bytes_serialized: tx_outs
                .checked_mul(ENTRY_LEN)
                .ok_or(TxOutSetArithmeticError {
                    field: "bytes_serialized",
                })?,
            hash_serialized: self.digest,
            total_zatoshis: net(self.zatoshis_added, self.zatoshis_removed, "total_zatoshis")?,
        })
    }

    fn xor(&mut self, digest: [u8; 32]) {
        for (byte, digest_byte) in self.digest.iter_mut().zip(digest) {
            *byte ^= digest_byte;
        }
    }

    fn create(&mut self, member: &SetMember) -> Result<(), TxOutSetArithmeticError> {
        self.outputs_added = add(self.outputs_added, 1, "outputs_added")?;
        self.zatoshis_added = add(self.zatoshis_added, member.value.as_u64(), "zatoshis_added")?;
        self.xor(member.digest);
        Ok(())
    }

    fn spend(&mut self, member: &SetMember) -> Result<(), TxOutSetArithmeticError> {
        self.outputs_removed = add(self.outputs_removed, 1, "outputs_removed")?;
        self.zatoshis_removed = add(
            self.zatoshis_removed,
            member.value.as_u64(),
            "zatoshis_removed",
        )?;
        self.xor(member.digest);
        Ok(())
    }
}

/// Per-index context: the block's transactions, each with its id and its
/// transparent inputs and outputs, in block order.
pub struct TxOutSetCtx {
    /// Block height.
    pub height: BlockHeight,
    /// The block's transaction ids, in block order.
    pub txids: Vec<TransactionId>,
    /// The block's transparent data, parallel to `txids`.
    pub txs: Vec<TransparentTxCompact>,
}

/// Delta: one block's change to the summary.
pub struct TxOutSetEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// The block's change.
    pub value: TxOutSetDelta,
}

/// Why extracting a block's summary change failed.
#[derive(Debug, thiserror::Error)]
pub enum TxOutSetError {
    /// Reading a dependency failed.
    #[error("reading a dependency of the UTXO-set summary")]
    Deps(#[source] DepsReadError),
    /// A running sum left its range.
    #[error(transparent)]
    Arithmetic(#[from] TxOutSetArithmeticError),
    /// An input spends a transaction `txid_location` does not know. The
    /// dependencies have persisted this batch and every earlier one, so on a
    /// valid chain this is an engine or store inconsistency.
    #[error("input at height {height} spends unknown transaction {txid:?}")]
    UnknownTransaction {
        /// The height of the spending block.
        height: BlockHeight,
        /// The transaction the input names.
        txid: TransactionId,
    },
    /// `txid_location` named a block or a position `transparent_data` does not
    /// hold, or the input names an output index the transaction does not have.
    #[error("no transparent output {index} of {txid:?} at height {height}")]
    MissingOutput {
        /// The height `txid_location` gave for the transaction.
        height: BlockHeight,
        /// The transaction the input names.
        txid: TransactionId,
        /// The output index the input names.
        index: OutputIndex,
    },
    /// The context's transaction ids and transparent data disagree in length.
    #[error("block at height {height} has {txids} transaction ids but {txs} transparent entries")]
    MisalignedContext {
        /// Block height.
        height: BlockHeight,
        /// Number of transaction ids.
        txids: usize,
        /// Number of transparent entries.
        txs: usize,
    },
    /// A block position did not fit `u32`.
    #[error("transaction position {position} at height {height} exceeds u32")]
    PositionOverflow {
        /// Block height.
        height: BlockHeight,
        /// The offending position.
        position: usize,
    },
}

impl From<DepsReadError> for TxOutSetError {
    fn from(err: DepsReadError) -> Self {
        Self::Deps(err)
    }
}

/// Index definition.
pub struct TxOutSetIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("txout_set");

impl IndexDef for TxOutSetIndex {
    type Scope = CrossIndex;
    type Composition = Append;
    type Delta = TxOutSetEntry;
    type BlockContext = TxOutSetCtx;

    const NAME: IndexId = ID;
    // `txid_location` and `transparent_data` resolve the output an input spends;
    // `transparent_spends` says whether its sibling outputs are spent.
    const DEPENDENCIES: &'static [IndexId] = &[
        txid_location::ID,
        transparent_data::ID,
        transparent_spends::ID,
    ];
}

/// Where a spend sits in the chain, for ordering it against other spends.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SpendPosition {
    height: BlockHeight,
    tx_index: u32,
}

/// The dependency reads of one block's extraction, with the per-block
/// transparent data memoised: a block spending many outputs of one earlier
/// block decodes that block once.
struct Resolver<'a, 'deps> {
    deps: &'a DepsReader<'deps>,
    blocks: HashMap<BlockHeight, TransparentBlockValue>,
}

impl<'a, 'deps> Resolver<'a, 'deps> {
    fn location(&self, txid: &TransactionId) -> Result<Option<TxLocation>, TxOutSetError> {
        Ok(self.deps.get::<TxidLocationIndex>(txid)?)
    }

    /// The transparent outputs of the transaction at `location`.
    fn outputs(
        &mut self,
        txid: TransactionId,
        location: &TxLocation,
        index: OutputIndex,
    ) -> Result<&[(Zatoshis, Script)], TxOutSetError> {
        let missing = || TxOutSetError::MissingOutput {
            height: location.height,
            txid,
            index,
        };
        if !self.blocks.contains_key(&location.height) {
            let block = self
                .deps
                .get::<TransparentDataIndex>(&location.height)?
                .ok_or_else(missing)?;
            self.blocks.insert(location.height, block);
        }
        let position = usize::try_from(location.tx_index).map_err(|_| missing())?;
        self.blocks
            .get(&location.height)
            .and_then(|block| block.0.get(position))
            .map(|tx| tx.outputs.as_slice())
            .ok_or_else(missing)
    }

    /// Whether the spend at `position` of output `spent_index` is the last spend
    /// of `txid`'s set members — the one that empties the transaction.
    ///
    /// True exactly when every other set member of the transaction is spent, and
    /// spent no later than this one. Ties inside one spending transaction go to
    /// the highest output index, so exactly one of its spends is the last.
    fn empties_transaction(
        &self,
        txid: TransactionId,
        siblings: &[OutputIndex],
        spent_index: OutputIndex,
        position: SpendPosition,
    ) -> Result<bool, TxOutSetError> {
        for &sibling in siblings {
            if sibling == spent_index {
                continue;
            }
            let Some(spender) = self.deps.get::<TransparentSpendsIndex>(&OutpointKey {
                prev_txid: txid,
                prev_index: sibling,
            })?
            else {
                return Ok(false);
            };
            let spender_location =
                self.location(&spender)?
                    .ok_or(TxOutSetError::UnknownTransaction {
                        height: position.height,
                        txid: spender,
                    })?;
            let sibling_position = SpendPosition {
                height: spender_location.height,
                tx_index: spender_location.tx_index,
            };
            if sibling_position > position
                || (sibling_position == position && sibling > spent_index)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl ExtractCross for TxOutSetIndex {
    type Error = TxOutSetError;

    fn extract(ctx: &TxOutSetCtx, deps: &DepsReader<'_>) -> Result<TxOutSetEntry, TxOutSetError> {
        if ctx.txids.len() != ctx.txs.len() {
            return Err(TxOutSetError::MisalignedContext {
                height: ctx.height,
                txids: ctx.txids.len(),
                txs: ctx.txs.len(),
            });
        }

        let mut delta = TxOutSetDelta::EMPTY;
        let mut resolver = Resolver {
            deps,
            blocks: HashMap::new(),
        };

        for (position, (txid, tx)) in ctx.txids.iter().zip(&ctx.txs).enumerate() {
            let tx_index =
                u32::try_from(position).map_err(|_| TxOutSetError::PositionOverflow {
                    height: ctx.height,
                    position,
                })?;

            let mut created_any = false;
            for (index, (value, script)) in tx.outputs.iter().enumerate() {
                let index =
                    OutputIndex::try_from(index).map_err(|_| TxOutSetError::PositionOverflow {
                        height: ctx.height,
                        position: index,
                    })?;
                if let Some(member) = set_member(*txid, index, *value, script) {
                    delta.create(&member)?;
                    created_any = true;
                }
            }
            if created_any {
                delta.transactions_added = add(delta.transactions_added, 1, "transactions_added")?;
            }

            for (prev_txid, prev_index) in &tx.inputs {
                // A coinbase input spends the null outpoint, which is no output.
                if <[u8; 32]>::from(*prev_txid) == [0u8; 32] {
                    continue;
                }
                let location =
                    resolver
                        .location(prev_txid)?
                        .ok_or(TxOutSetError::UnknownTransaction {
                            height: ctx.height,
                            txid: *prev_txid,
                        })?;
                let outputs = resolver.outputs(*prev_txid, &location, *prev_index)?;

                let mut members = Vec::new();
                let mut spent = None;
                for (index, (value, script)) in outputs.iter().enumerate() {
                    let index = OutputIndex::try_from(index).map_err(|_| {
                        TxOutSetError::PositionOverflow {
                            height: location.height,
                            position: index,
                        }
                    })?;
                    if let Some(member) = set_member(*prev_txid, index, *value, script) {
                        members.push(index);
                        if index == *prev_index {
                            spent = Some(member);
                        }
                    }
                }
                if usize::try_from(*prev_index).map_or(true, |index| index >= outputs.len()) {
                    return Err(TxOutSetError::MissingOutput {
                        height: location.height,
                        txid: *prev_txid,
                        index: *prev_index,
                    });
                }
                // An output outside the set was never added, so spending it
                // removes nothing.
                let Some(spent) = spent else { continue };

                delta.spend(&spent)?;
                if resolver.empties_transaction(
                    *prev_txid,
                    &members,
                    *prev_index,
                    SpendPosition {
                        height: ctx.height,
                        tx_index,
                    },
                )? {
                    delta.transactions_removed =
                        add(delta.transactions_removed, 1, "transactions_removed")?;
                }
            }
        }

        Ok(TxOutSetEntry {
            height: ctx.height,
            value: delta,
        })
    }
}

impl MergeAppend for TxOutSetIndex {}

impl Schema<Vec<TxOutSetEntry>> for TxOutSetIndex {
    fn into_entries(entries: Vec<TxOutSetEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TxOutSetEntry> {
        entries
            .into_iter()
            .map(|(height, value)| TxOutSetEntry { height, value })
            .collect()
    }
}

impl EntryCodec for TxOutSetIndex {
    type Key = BlockHeight;
    type Value = TxOutSetDelta;
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentTxOutSetDelta;

    const KEY_ORDER: KeyOrder = KeyOrder::WalkOrdered;

    fn fingerprint_samples() -> Vec<(BlockHeight, TxOutSetDelta)> {
        vec![(
            BlockHeight::new(1),
            TxOutSetDelta {
                transactions_added: 2,
                transactions_removed: 3,
                outputs_added: 4,
                outputs_removed: 5,
                zatoshis_added: 6,
                zatoshis_removed: 7,
                digest: [8; 32],
            },
        )]
    }
}

/// On-disk delta record: six `u64` running sums (LE) then the 32-byte XOR
/// digest = 80 bytes.
#[derive(PersistentRecord)]
pub struct PersistentTxOutSetDelta {
    transactions_added: u64,
    transactions_removed: u64,
    outputs_added: u64,
    outputs_removed: u64,
    zatoshis_added: u64,
    zatoshis_removed: u64,
    digest: [u8; 32],
}

impl PersistentRecord for PersistentTxOutSetDelta {
    type Domain = TxOutSetDelta;

    fn from_domain(domain: &TxOutSetDelta) -> Self {
        Self {
            transactions_added: domain.transactions_added,
            transactions_removed: domain.transactions_removed,
            outputs_added: domain.outputs_added,
            outputs_removed: domain.outputs_removed,
            zatoshis_added: domain.zatoshis_added,
            zatoshis_removed: domain.zatoshis_removed,
            digest: domain.digest,
        }
    }

    fn into_domain(self) -> Result<TxOutSetDelta, DecodeError> {
        Ok(TxOutSetDelta {
            transactions_added: self.transactions_added,
            transactions_removed: self.transactions_removed,
            outputs_added: self.outputs_added,
            outputs_removed: self.outputs_removed,
            zatoshis_added: self.zatoshis_added,
            zatoshis_removed: self.zatoshis_removed,
            digest: self.digest,
        })
    }
}
