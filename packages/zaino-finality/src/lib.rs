//! `zaino-finality` — the single-owner contract for the seam between Zaino's
//! durable tier and its volatile one.
//!
//! The whole contract is one chain of inequalities, where `t` is the
//! validator's tip, `d` the consensus reorg depth, and `margin` the retention
//! overlap:
//!
//! ```text
//! genesis  <=  floor + margin  <=  w  <=  r  <=  t - d  <=  t
//! ```
//!
//! Each link has exactly one party permitted to move its left-hand side, and
//! that is what the two halves of a [`Seam`] encode:
//!
//! ```text
//! r      volatile tier, bounded by t - d       (its own graph)
//! w      durable tier,  bounded by r           (read across the seam)
//! floor  volatile tier, bounded by w - margin  (read across the seam)
//! ```
//!
//! `r` is derived, not chosen:
//!
//! ```text
//! r = height(t) - d        t = the volatile tier's canonical tip
//! ```
//!
//! It depends only on `t`, which is external to the seam, so the ratchet has no
//! cycle. Conditioning it on anything the volatile tier retains — competing
//! branches it has not yet swept, say — would make `r` depend on retention,
//! retention on `floor`, and `floor` on `w <= r`, closing one.
//!
//! So the seam carries exactly two quantities, `r` and `w`. `floor` is internal
//! to the volatile tier and never crosses.
//!
//! Gapless served coverage is a consequence of the first link rather than a
//! separate rule:
//!
//! ```text
//! [genesis, w] union [floor, t] = [genesis, t]    <==    floor <= w
//! ```
//!
//! # Why the types and not a pair of channels
//!
//! The relation `w <= r` spans both tiers, so neither tier can check it alone
//! without the check being written twice and forgettable in either. The seam
//! holds it in one place, applied on every publish.
//!
//! Each half publishes one quantity and reads the other; neither is `Clone` and
//! both publish through `&mut self`, so single-writer holds at the borrow
//! checker as well as at the type level. [`Released`] has no public
//! constructor, and [`DurableWatermark::advance`] requires one, so the durable
//! tier cannot advance on its own authority.

#[cfg(test)]
mod tests;

mod fault;
mod seam;

pub use fault::SeamFault;
pub use seam::{
    Committed, DurableWatermark, HorizonReader, Released, ReorgHorizon, Seam,
    DEFAULT_RETENTION_MARGIN,
};
