//! BlockLocal × Monoidal: counts total blocks seen in each batch.

use crate::descriptor::{BlockLocal, Monoidal};
use crate::primitives::IndexId;
use crate::traits::{ExtractError, ExtractLocal, IndexDef, MergeMonoidal, Schema};

/// Block context for this index: nothing needed.
///
/// CountIndex only counts blocks — it reads no data from the block.
/// Using `()` means any set-wide context satisfies it via a trivial
/// `ProvideContext<()>` impl.
pub type Context = ();

/// Counts total blocks seen in each batch.
pub struct CountIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("count");

impl IndexDef for CountIndex {
    type Scope = BlockLocal;
    type Composition = Monoidal;
    type Delta = u64;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractLocal for CountIndex {
    fn extract(_ctx: &Context) -> Result<Self::Delta, ExtractError> {
        Ok(1)
    }
}

impl MergeMonoidal for CountIndex {
    type Accumulator = u64;

    fn identity() -> Self::Accumulator {
        0
    }

    fn lift(delta: Self::Delta) -> Self::Accumulator {
        delta
    }

    fn combine(a: Self::Accumulator, b: Self::Accumulator) -> Self::Accumulator {
        a + b
    }
}

impl Schema<u64> for CountIndex {
    type Key = &'static [u8];
    type Value = u64;

    fn into_entries(count: u64) -> Vec<(Self::Key, Self::Value)> {
        vec![(b"total".as_slice(), count)]
    }
}
