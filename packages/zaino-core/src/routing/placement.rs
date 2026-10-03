//! Placements, as types: where a capability is answered.
//!
//! Exactly three implementors of the sealed [`Placement`] trait — [`Local`],
//! [`Passthrough`], [`Withheld`] — one per value of [`PlacementKind`]. They are
//! the `Self` types the composer's per-capability placement traits dispatch on;
//! being distinct types, those impls cannot overlap, and [`Withheld`] implements
//! none of them.

use super::PlacementKind;

mod sealed {
    pub trait Sealed {}
}

/// A placement, as a type. Exactly three implementors: [`Local`], [`Passthrough`],
/// [`Withheld`].
pub trait Placement: sealed::Sealed + Send + Sync + 'static {
    /// The same placement, as a value — for the manifest derivation.
    const KIND: PlacementKind;
}

/// Answered from the local chain tiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Local;
/// Answered live by the validator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Passthrough;
/// Not offered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Withheld;

impl sealed::Sealed for Local {}
impl sealed::Sealed for Passthrough {}
impl sealed::Sealed for Withheld {}

impl Placement for Local {
    const KIND: PlacementKind = PlacementKind::Local;
}
impl Placement for Passthrough {
    const KIND: PlacementKind = PlacementKind::Passthrough;
}
impl Placement for Withheld {
    const KIND: PlacementKind = PlacementKind::Withheld;
}
