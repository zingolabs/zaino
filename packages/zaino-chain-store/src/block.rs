//! Blocks as the store holds and serves them.

use zaino_primitives::types::{
    BlockHeader, BlockRef, ChainWork, PreIndexCompactTx, ShieldedPool, SignedZatoshis, TreeRoots,
};

/// A finalised block, as the store indexed it.
///
/// A projection, not the block. The store keeps the fields an index reads —
/// the header, per-transaction compact data, the commitment tree state after
/// the block — and not the consensus bytes, so this cannot reproduce what the
/// block hash commits to. Raw blocks and raw transactions are served from the
/// validator, and that is a property of the design rather than a gap: storing
/// full blocks would multiply the database for data the validator already
/// holds.
///
/// Shaped deliberately like `zaino_chain_head::ChainHeadBlock`, so a consumer
/// routing across the finalised/recent seam meets one shape rather than two.
/// The two differ in exactly two ways, and both differences are real:
///
/// - `chainwork` here is absolute, measured from genesis. The chain head's is
///   measured from its own anchor, because it never reads the finalised state
///   and so cannot know the absolute value.
/// - `transactions` here are compact. The chain head retains parsed
///   transactions, because it takes them from the validator whole.
///
/// Not comparable: its transaction and tree-root types are not, and matching
/// `ChainHeadBlock`, which is not either. A test wanting to compare two blocks
/// compares the fields it cares about.
#[derive(Debug, Clone)]
pub struct StoredBlock {
    /// The block's header: its hash, parent, height, time, difficulty, roots
    /// and nonce.
    pub header: BlockHeader,
    /// Per-transaction indexed data, in block order.
    pub transactions: Vec<StoredTx>,
    /// Commitment tree roots and sizes *after* this block is applied.
    ///
    /// Roots as well as sizes, where [`CompactBlock`] carries only sizes: the
    /// roots are what an index needs and a compact block does not.
    pub tree_roots: TreeRoots,
    /// Cumulative work from genesis to this block.
    pub chainwork: ChainWork,
}

/// A transaction in a stored block.
///
/// A compact transaction, plus the per-pool value balances that sit beside it
/// in the index.
///
/// # Why not [`PreIndexCompactTx`] alone
///
/// That type is the light-wallet compact projection, and the compact protocol
/// carries no value balance — correctly, because a wallet does not need one. An
/// index does: the balances are a persisted field, so a block expressed only as
/// compact transactions cannot be written back without inventing them, and a
/// block read out of a store and written into another would silently lose them.
///
/// That is not hypothetical. It is exactly what [`StoredBlock`] is for — it is
/// what [`ChainStoreFreezeSink`](crate::ChainStoreFreezeSink) takes and what
/// [`StoredBlockRead`](crate::StoredBlockRead) yields — so the read and the
/// write have to describe the same block or the port is lossy in the one
/// direction that writes to disk.
/// Not `PartialEq`, because [`PreIndexCompactTx`] is not: comparing two blocks
/// field-by-field is a test's business, and the fields are public.
#[derive(Debug, Clone)]
pub struct StoredTx {
    /// The compact transaction: identifiers, transparent movement, and the
    /// shielded components a wallet scans.
    pub compact: PreIndexCompactTx,
    /// Net Sapling value balance, or `None` where the transaction has no
    /// Sapling component.
    ///
    /// `Option` rather than a zero, because the two are distinguishable on disk
    /// and a store that conflated them would rewrite rows it only meant to read.
    pub sapling_value: Option<SignedZatoshis>,
    /// Net Orchard value balance, or `None` where there is no Orchard
    /// component.
    pub orchard_value: Option<SignedZatoshis>,
    /// Net Ironwood value balance, or `None` where there is no Ironwood
    /// component.
    pub ironwood_value: Option<SignedZatoshis>,
}

impl StoredTx {
    /// A transaction with no shielded value movement.
    ///
    /// The common case: every transparent-only transaction, and every coinbase.
    pub fn transparent_only(compact: PreIndexCompactTx) -> Self {
        Self {
            compact,
            sapling_value: None,
            orchard_value: None,
            ironwood_value: None,
        }
    }
}

impl StoredBlock {
    /// This block's height and hash.
    pub fn reference(&self) -> BlockRef {
        BlockRef {
            height: self.header.height,
            hash: self.header.hash,
        }
    }
}

/// Which pools a block read should include.
///
/// Pushed into the read rather than applied to the result. The store keeps
/// each pool's per-transaction data separately, so a filter lets it skip
/// reading and decoding what the caller will not look at — which on a range
/// query is the difference between touching one pool's data and touching all
/// of them. Filtering afterwards would do the work anyway and then discard it.
///
/// The default is every shielded pool and no transparent data, matching what a
/// light wallet asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolFilter {
    transparent: bool,
    sapling: bool,
    orchard: bool,
    ironwood: bool,
}

impl Default for PoolFilter {
    fn default() -> Self {
        Self {
            transparent: false,
            sapling: true,
            orchard: true,
            ironwood: true,
        }
    }
}

impl PoolFilter {
    /// Every pool, transparent included.
    pub fn all() -> Self {
        Self {
            transparent: true,
            sapling: true,
            orchard: true,
            ironwood: true,
        }
    }

    /// No pool at all — header and txids only.
    pub fn none() -> Self {
        Self {
            transparent: false,
            sapling: false,
            orchard: false,
            ironwood: false,
        }
    }

    /// The same filter, with transparent data included.
    pub fn with_transparent(mut self) -> Self {
        self.transparent = true;
        self
    }

    /// The same filter, with `pool` included.
    pub fn with_pool(mut self, pool: ShieldedPool) -> Self {
        match pool {
            ShieldedPool::Sapling => self.sapling = true,
            ShieldedPool::Orchard => self.orchard = true,
            ShieldedPool::Ironwood => self.ironwood = true,
        }
        self
    }

    /// Whether transparent data is included.
    pub fn includes_transparent(&self) -> bool {
        self.transparent
    }

    /// Whether `pool` is included.
    pub fn includes(&self, pool: ShieldedPool) -> bool {
        match pool {
            ShieldedPool::Sapling => self.sapling,
            ShieldedPool::Orchard => self.orchard,
            ShieldedPool::Ironwood => self.ironwood,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is what a light wallet asks for: shielded pools, no
    /// transparent data. Pinned because flipping it would silently change what
    /// every unfiltered range read costs.
    #[test]
    fn the_default_filter_is_shielded_only() {
        let filter = PoolFilter::default();
        assert!(!filter.includes_transparent());
        assert!(filter.includes(ShieldedPool::Sapling));
        assert!(filter.includes(ShieldedPool::Orchard));
        assert!(filter.includes(ShieldedPool::Ironwood));
    }

    #[test]
    fn none_includes_nothing_and_all_includes_everything() {
        let none = PoolFilter::none();
        assert!(!none.includes_transparent());
        assert!(!none.includes(ShieldedPool::Sapling));

        let all = PoolFilter::all();
        assert!(all.includes_transparent());
        assert!(all.includes(ShieldedPool::Ironwood));
    }

    #[test]
    fn pools_can_be_added_one_at_a_time() {
        let filter = PoolFilter::none()
            .with_pool(ShieldedPool::Orchard)
            .with_transparent();
        assert!(filter.includes(ShieldedPool::Orchard));
        assert!(filter.includes_transparent());
        assert!(!filter.includes(ShieldedPool::Sapling));
    }
}
