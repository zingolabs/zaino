//! In-memory stand-in for the non-finalised side of the seam.
//!
//! [`StubNonFinalised`] holds a fixed set of best-chain compact blocks over
//! `[floor, tip]` and answers the shared `zaino-service` segment ports against
//! them. It stands in for the real `ChainGraph`-backed head, so the FS⊕NFS route
//! can be exercised end to end without wiring the volatile graph.
//!
//! A fixed window is already its own pinned view: it never advances, so
//! [`TakeSnapshot::snapshot`] just clones it. The real chain-head, which does
//! advance, captures a fresh snapshot per pin.

use std::collections::BTreeMap;
use std::future::Future;

use futures::stream::{self, BoxStream, StreamExt};

use zaino_address::script_pays;
use zaino_primitives::types::CompactDifficulty;
use zaino_primitives::types::{
    BlockHash, BlockRef, BlockSelector, ChainMetadata, CompactBlock, Height, HeightRange,
};
use zaino_primitives::types::{
    Outpoint, OutputIndex, PreIndexCompactTx, TransparentAddress, TransparentReceive,
    TransparentSpend,
};
use zaino_primitives::types::{ShieldedPool, SubtreeRoot, Treestate};
use zaino_service::error::{
    AddressReadError, BlockReadError, ReadError, SpendReadError, Transient, TreestateReadError,
};
use zaino_service::{
    AddressReceiveRead, Capability, ChainSegment, CompactBlockRead, HeaderRead, HeaderSummary,
    SpendRead, SpendStatus, TakeSnapshot, TreestateRead, TreestateWindowRead,
};

/// A fixed non-finalised window backed by an in-memory map.
///
/// Retains best-chain compact blocks keyed by height; the tip is the
/// highest-height block and the floor the lowest. Cheap to clone (the composer
/// clones the view into every snapshot).
#[derive(Clone, Debug, Default)]
pub struct StubNonFinalised {
    /// Best-chain compact blocks over `[floor, tip]`, keyed by height.
    blocks: BTreeMap<Height, CompactBlock>,
    /// The highest-height block's id, or `None` when empty.
    tip: Option<BlockRef>,
}

impl StubNonFinalised {
    /// An empty window — the head holds nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A window over `blocks`. The floor and tip are derived from the contents
    /// (lowest and highest height); blocks whose height exceeds the protocol
    /// limit are dropped rather than retained.
    pub fn from_blocks(blocks: Vec<CompactBlock>) -> Self {
        let blocks: BTreeMap<Height, CompactBlock> = blocks
            .into_iter()
            .filter_map(|block| {
                Height::try_from(block.height)
                    .ok()
                    .map(|height| (height, block))
            })
            .collect();
        let tip = blocks.last_key_value().map(|(height, block)| BlockRef {
            height: *height,
            hash: block.hash,
        });
        Self { blocks, tip }
    }
}

impl ChainSegment for StubNonFinalised {
    fn pinned_tip(&self) -> Option<BlockRef> {
        self.tip
    }

    fn coverage(&self) -> Option<HeightRange> {
        let start = *self.blocks.keys().next()?;
        let end = *self.blocks.keys().next_back()?;
        Some(HeightRange { start, end })
    }
}

impl CompactBlockRead for StubNonFinalised {
    fn compact_block(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<CompactBlock>, BlockReadError>> + Send {
        // Best-effort over the stored window: a height maps directly, a hash
        // matches any stored block. Reads never fail in the stub.
        let block = match at {
            BlockSelector::Height(height) => self.blocks.get(&height).cloned(),
            BlockSelector::Hash(hash) => self
                .blocks
                .values()
                .find(|block| block.hash == hash)
                .cloned(),
        };
        std::future::ready(Ok(block))
    }

    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        let start = u32::from(range.start);
        let end = u32::from(range.end);
        let blocks: Vec<Result<CompactBlock, ReadError>> = (start..=end)
            .filter_map(|height| Height::try_from(height).ok())
            .filter_map(|height| self.blocks.get(&height).cloned())
            .map(Ok)
            .collect();
        stream::iter(blocks).boxed()
    }
}

impl HeaderRead for StubNonFinalised {
    fn header(
        &self,
        h: Height,
    ) -> impl Future<Output = Result<Option<HeaderSummary>, BlockReadError>> + Send {
        // Projected from the stored window: a height the window holds yields its
        // block's hash and time; an absent height is a domain miss. Reads never
        // fail in the stub.
        let summary = self.blocks.get(&h).map(|block| HeaderSummary {
            hash: block.hash,
            time: block.time,
        });
        std::future::ready(Ok(summary))
    }
}

impl TakeSnapshot for StubNonFinalised {
    type Snapshot = Self;

    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send {
        std::future::ready(Ok(self.clone()))
    }
}

/// A filler compact block at `height` whose hash is `[hash_byte; 32]`.
///
/// Only the height and hash carry meaning for routing tests; the remaining
/// fields are inert placeholders.
/// The window answers spend status from its compact transactions, the same way
/// the real head does and over the same fields: a compact transaction carries
/// the outpoints its inputs spend and the outputs it creates, which is
/// everything this read needs.
///
/// Mirrors `zaino_chain_head_service`'s implementation, including its reading
/// of `NoSuchOutput` as "not in this window" rather than "nowhere" — the
/// composer falls through to the finalised store on anything but a spend.
impl SpendRead for StubNonFinalised {
    async fn spend_status(&self, outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        let mut created = false;
        for block in self.blocks.values() {
            for transaction in &block.transactions {
                if spends(transaction, outpoint) {
                    return Ok(SpendStatus::Spent {
                        by: transaction.txid,
                    });
                }
                created |= creates(transaction, outpoint);
            }
        }
        Ok(match created {
            true => SpendStatus::Unspent,
            false => SpendStatus::NoSuchOutput,
        })
    }

    async fn spend_info(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TransparentSpend>, SpendReadError> {
        for (height, block) in &self.blocks {
            for (block_index, transaction) in enumerated(&block.transactions) {
                let found = transaction
                    .transparent_inputs
                    .iter()
                    .enumerate()
                    .find(|(_, input)| {
                        input.prev_txid == outpoint.txid && input.prev_index == outpoint.index
                    });
                if let Some((index, _)) = found {
                    let Ok(input_index) = OutputIndex::try_from(index) else {
                        continue;
                    };
                    return Ok(Some(TransparentSpend {
                        outpoint,
                        by: transaction.txid,
                        input_index,
                        height: *height,
                        block_index,
                    }));
                }
            }
        }
        Ok(None)
    }
}

/// The window reports the receives it holds, deriving each output's recipient
/// from its script, exactly as the real head does.
///
/// Mirrors `zaino_chain_head_service`'s implementation, including why this is
/// the narrower read: a compact input carries only the outpoint it spends, so a
/// spend cannot be attributed to an address here.
impl AddressReceiveRead for StubNonFinalised {
    async fn receives(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransparentReceive>, AddressReadError> {
        let mut receives = Vec::new();
        // The map is height-keyed and ordered, so the range selects a run and
        // walking it yields height order.
        for (height, block) in self.blocks.range(range.start..=range.end) {
            for (block_index, transaction) in enumerated(&block.transactions) {
                for (index, output) in transaction.transparent_outputs.iter().enumerate() {
                    if !script_pays(output.script.as_bytes(), addr) {
                        continue;
                    }
                    let Ok(output_index) = OutputIndex::try_from(index) else {
                        continue;
                    };
                    receives.push(TransparentReceive {
                        txid: transaction.txid,
                        output_index,
                        script: output.script.clone(),
                        value: output.value,
                        height: *height,
                        block_index,
                    });
                }
            }
        }
        Ok(receives)
    }

    async fn spends(
        &self,
        outpoints: &[Outpoint],
        range: HeightRange,
    ) -> Result<Vec<TransparentSpend>, AddressReadError> {
        let mut spends = Vec::new();
        for (height, block) in self.blocks.range(range.start..=range.end) {
            for (block_index, transaction) in enumerated(&block.transactions) {
                for (index, input) in transaction.transparent_inputs.iter().enumerate() {
                    let Some(outpoint) = outpoints.iter().find(|outpoint| {
                        input.prev_txid == outpoint.txid && input.prev_index == outpoint.index
                    }) else {
                        continue;
                    };
                    let Ok(input_index) = OutputIndex::try_from(index) else {
                        continue;
                    };
                    spends.push(TransparentSpend {
                        outpoint: *outpoint,
                        by: transaction.txid,
                        input_index,
                        height: *height,
                        block_index,
                    });
                }
            }
        }
        Ok(spends)
    }
}

impl StubNonFinalised {
    /// A canned treestate at a height the window holds — block identity from the
    /// stored block, every pool absent. Enough to stand in for a tier under a
    /// `Local` treestate placement without computing real frontiers (this crate
    /// has no commitment-tree algebra).
    fn scripted_treestate(&self, at: Height) -> Option<Treestate> {
        self.blocks.get(&at).map(|block| Treestate {
            block_hash: block.hash,
            height: at,
            time: block.time,
            sapling: None,
            orchard: None,
            ironwood: None,
        })
    }
}

/// The finalised-tier treestate read, so the stub can stand in as the `F` side of
/// a `Local` placement. A height the window holds answers; above it is
/// `NotServiceable`, never an empty tree.
impl TreestateRead for StubNonFinalised {
    async fn treestate(&self, at: Height) -> Result<Treestate, TreestateReadError> {
        self.scripted_treestate(at)
            .ok_or(TreestateReadError::NotServiceable(Capability::Treestate))
    }

    async fn subtree_roots(
        &self,
        _pool: ShieldedPool,
        _start_index: u16,
        _limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        Ok(Vec::new())
    }
}

/// The window-tier treestate read, so the stub can stand in as the `N` side. A
/// height the window holds answers; above it is a domain miss.
impl TreestateWindowRead for StubNonFinalised {
    async fn window_treestate(
        &self,
        _seed: Option<&Treestate>,
        at: Height,
    ) -> Result<Option<Treestate>, TreestateReadError> {
        Ok(self.scripted_treestate(at))
    }

    async fn window_subtree_roots(
        &self,
        _seed: Option<&Treestate>,
        _pool: ShieldedPool,
        _start_index: u16,
        _limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        Ok(Vec::new())
    }
}

/// A block's transactions paired with their in-block position as a `u32`,
/// mirroring the real head: a position past `u32` cannot occur in a block that
/// parsed, so that transaction is skipped rather than failing the infallible read.
fn enumerated(
    transactions: &[PreIndexCompactTx],
) -> impl Iterator<Item = (u32, &PreIndexCompactTx)> {
    transactions
        .iter()
        .enumerate()
        .filter_map(|(index, transaction)| Some((u32::try_from(index).ok()?, transaction)))
}

/// Whether `transaction` spends `outpoint`.
fn spends(transaction: &PreIndexCompactTx, outpoint: Outpoint) -> bool {
    transaction
        .transparent_inputs
        .iter()
        .any(|input| input.prev_txid == outpoint.txid && input.prev_index == outpoint.index)
}

/// Whether `transaction` created `outpoint`.
fn creates(transaction: &PreIndexCompactTx, outpoint: Outpoint) -> bool {
    if transaction.txid != outpoint.txid {
        return false;
    }
    usize::try_from(outpoint.index).is_ok_and(|index| index < transaction.transparent_outputs.len())
}

pub fn stub_compact_block(height: u32, hash_byte: u8) -> CompactBlock {
    CompactBlock {
        hash: BlockHash::from([hash_byte; 32]),
        prev_hash: BlockHash::from([hash_byte.wrapping_sub(1); 32]),
        height,
        time: 0,
        bits: CompactDifficulty::try_from_bits(0x2007_ffff)
            .expect("0x2007ffff is a valid nBits encoding"),
        transactions: Vec::new(),
        chain_metadata: ChainMetadata::ZERO,
    }
}
