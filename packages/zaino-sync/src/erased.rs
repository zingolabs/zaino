//! Type-erased index interface for the engine.
//!
//! The trait hierarchy in [`crate::traits`] gives compile-time safety at the
//! index *definition* site. The engine, however, must handle all 9 cells of
//! the (Scope × Composition) grid uniformly through dynamic dispatch. This
//! module bridges the two: blanket impls on concrete (Scope, Composition)
//! pairs produce [`ErasedIndex`] trait objects that the engine stores and
//! dispatches through.
//!
//! The engine never inspects `Delta` or `Accumulator` — it shuttles opaque
//! values between extract → merge → write_ops. Type erasure loses nothing
//! the engine needs.

use std::any::Any;

use crate::descriptor::Descriptor;
use crate::traits::{ExtractError, WriteOp};

/// Opaque delta produced by extraction, carrying the concrete `Delta` type
/// inside a `Box<dyn Any>`.
pub struct ErasedDelta(pub(crate) Box<dyn Any + Send + Sync>);

/// Type-erased interface the engine dispatches through.
///
/// One `Box<dyn ErasedIndex>` per registered index. The engine uses
/// [`Descriptor`] to decide scheduling, then calls the erased methods
/// without knowing the concrete types.
pub trait ErasedIndex: Send + Sync {
    /// The declarative descriptor.
    fn descriptor(&self) -> &Descriptor;

    /// Extract a delta for one block.
    ///
    /// The engine provides the right inputs based on `descriptor().scope`:
    /// - `BlockLocal`: `ctx` only (prior_state and deps are None).
    /// - `SelfCumulative`: `ctx` + `prior_state`.
    /// - `CrossIndex`: `ctx` + `deps`.
    ///
    /// Passing the wrong combination is a bug in the engine, not the index.
    fn extract_erased(
        &self,
        ctx: &dyn Any,
        prior_state: Option<&dyn Any>,
        deps: Option<&crate::traits::DepsReader>,
    ) -> Result<ErasedDelta, ExtractError>;

    /// Merge a batch of deltas into write operations.
    ///
    /// The engine calls this based on `descriptor().composition`:
    /// - `Append`: deltas are simply flattened.
    /// - `Monoidal`: deltas are reduced with the declared monoid.
    /// - `Fold`: deltas are folded in chain order.
    ///
    /// `deltas` is in chain order regardless of composition type.
    fn merge_erased(&self, deltas: Vec<ErasedDelta>) -> Result<Vec<WriteOp>, MergeError>;
}

/// Errors during the merge step.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    /// A delta's concrete type didn't match what this index expected.
    /// Indicates an engine bug (wrong delta routed to wrong index).
    #[error("delta type mismatch: expected {expected}")]
    DeltaTypeMismatch {
        /// The index name that expected a different delta type.
        expected: &'static str,
    },
    /// The merge logic itself failed.
    #[error("merge failed: {0}")]
    Failed(String),
}

// ===========================================================================
// Blanket implementations — one per (Scope × Composition) cell.
//
// These connect the typed trait hierarchy to the erased interface.
// The engine never sees the concrete types; it works entirely through
// `Box<dyn ErasedIndex>`.
//
// Only the cells that have a concrete index registered will have an
// ErasedIndex instance. The blanket impls ensure that any type
// satisfying the right trait bounds automatically becomes erasable.
// ===========================================================================

// TODO: blanket impls for each (Scope, Composition) pair.
//
// Each blanket impl will:
// 1. Store a PhantomData<I> for the concrete index type.
// 2. In extract_erased: downcast `ctx` to `I::Context`, call the
//    scope-specific extract, box the resulting Delta.
// 3. In merge_erased: downcast each ErasedDelta to `I::Delta`, call
//    the composition-specific merge, return WriteOps.
//
// Example shape (not yet implemented):
//
//   struct ErasedLocalAppend<I>(PhantomData<I>);
//
//   impl<I> ErasedIndex for ErasedLocalAppend<I>
//   where
//       I: ExtractLocal + MergeAppend,
//   {
//       fn descriptor(&self) -> &Descriptor { ... }
//       fn extract_erased(...) -> ... { ... }
//       fn merge_erased(...) -> ... { ... }
//   }
