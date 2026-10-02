# Finality Seam Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the FS/NFS boundary exactly one owner per quantity, enforced by types rather than by configuration discipline.

**Architecture:** A new `zaino-finality` crate holds a `Seam` that owns the reorg depth, the retention margin, and both legs of the ratchet. It splits into two non-`Clone` halves: `ReorgHorizon` (held by the chain-head service, publishes `r`, reads `w`) and `DurableWatermark` (held by the sync engine, publishes `w`, reads `r`). `Released` is unforgeable and gates `DurableWatermark::advance`, so the durable tier cannot advance without the volatile tier's authorisation. `zaino-runtime::boot` is the only crate that constructs a seam.

**Tech Stack:** Rust (workspace edition), `tokio::sync::watch` for both legs, `thiserror` for faults, `cargo nextest` for tests, `cargo-make` (`makers`) for lints.

**Spec:** `docs/notes/finality-seam.md`

## Global Constraints

- Every crate carries `[lints.rust] unsafe_code = "forbid"` and `missing_docs = "warn"`. New public items need doc comments or the build warns.
- No `mod.rs`. Use `module.rs` + `module/` (see `packages/zaino-sync/src/testing.rs` + `testing/`).
- No `as` casts for numeric conversion. Use `From`/`Into`/`TryFrom`.
- Changesets: every PR touching a governed crate's source needs one entry per public change in `.changesets/`, created with `relman changeset new`. Format: `docs/release/changeset-format.md`.
- Mocks live in the crate they mock, behind a `testing` feature (pattern: `zaino-sync`'s `testing` module).
- Error types use typed causes via `#[source]`; never stringify, `expect`, or swallow.
- `Height` and `BlockHash` come from `zaino_primitives::types`.
- **The algebra is load-bearing documentation and must not be trimmed.** The inequality chain, the ownership table, the `r = height(t) - d` derivation, and the gapless-coverage consequence appear verbatim in `packages/zaino-finality/src/lib.rs` (Task 1, Step 6) and again in `packages/zaino-finality/usage.md` (Task 1, Step 9). They are the specification; the types are its transcription. Do not shorten, paraphrase, or move them to a commit message.
- Base branch: `refactor/store-service-split`. Work branch: `feat/finality-seam`. Worktree: `/home/chona/zingo/zingolabs/zaino/finality-seam`.
- `makers set-worktree-parent-tools` and `makers use-system-rocksdb` do not exist on this branch (exit 404). Expect a vendored rocksdb compile on the first build; do not treat the long first build as a fault.

## Review Focus

Five conditions the spec implies that no task's happy path exercises. Each has a test added to the owning task.

1. **First publish on an empty chain** — both quantities start `None`; `retention_floor()` must be `None` (not genesis-minus-margin underflow) and `DurableWatermark::advance` must reject with no `Released` yet. Task 1.
2. **`retention_margin` exceeding the current watermark** — `w = 3`, `margin = 10`: `retention_floor()` must saturate at genesis, not underflow. Task 1.
3. **A rejected publish must leave the state unchanged** — a fault must not half-apply, or the next legal publish is validated against a corrupted baseline. Task 1.
4. **Equal-value re-publish** — `advance` to the height already held. Monotonicity is non-decreasing, so this is legal and idempotent, not a regression fault. Task 1.
5. **Driver fetches a block whose hash differs from `Released.hash`** — must refuse before any write, not index divergent data and report afterwards. Task 3.

---

### Task 1: The `zaino-finality` crate

**Files:**
- Create: `packages/zaino-finality/Cargo.toml`
- Create: `packages/zaino-finality/src/lib.rs`
- Create: `packages/zaino-finality/src/seam.rs`
- Create: `packages/zaino-finality/src/fault.rs`
- Create: `packages/zaino-finality/src/tests.rs`
- Create: `packages/zaino-finality/usage.md`
- Modify: `Cargo.toml` (workspace `members`, `workspace.dependencies`)
- Create: `.changesets/finality-seam.toml`

**Interfaces:**
- Consumes: nothing.
- Produces: `Seam::new(reorg_depth: u32, retention_margin: u32) -> Seam`; `Seam::split(self) -> (ReorgHorizon, DurableWatermark)`; `ReorgHorizon::{reorg_depth() -> u32, advance(&mut self, tip: Height, hash_at_horizon: BlockHash) -> Result<Released, SeamFault>, durable() -> Option<Committed>, retention_floor() -> Option<Height>}`; `DurableWatermark::{released() -> Option<Released>, await_released(&mut self) -> impl Future<Output = Released>, advance(&mut self, authorised_by: &Released, to: Height) -> Result<Committed, SeamFault>}`; `Released::{height() -> Height, hash() -> BlockHash}`; `Committed::height() -> Height`; `SeamFault::{RegressedHorizon, RegressedWatermark, WatermarkPastHorizon}`.

- [ ] **Step 1: Create the crate manifest and register it in the workspace**

`packages/zaino-finality/Cargo.toml`:

```toml
[package]
name = "zaino-finality"
description = "The single-owner contract for Zaino's finalised/non-finalised seam."
authors = { workspace = true }
repository = { workspace = true }
homepage = { workspace = true }
edition = { workspace = true }
license = { workspace = true }
version = "0.1.0"

[dependencies]
zaino-primitives = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true, features = ["sync"] }

[lints.rust]
unsafe_code = "forbid"
missing_docs = "warn"
```

In the root `Cargo.toml`, add `"packages/zaino-finality",` to `[workspace] members` immediately after `"packages/zaino-consensus",`, and add to `[workspace.dependencies]`:

```toml
zaino-finality = { version = "0.1.0", path = "packages/zaino-finality" }
```

- [ ] **Step 2: Write the failing tests**

`packages/zaino-finality/src/tests.rs`:

```rust
//! Trace tests: the seam's invariants hold across sequences of publishes, so
//! each test drives a sequence and asserts the chain after every step.

use zaino_primitives::types::{BlockHash, Height};

use crate::{Seam, SeamFault};

/// A distinct hash per height, so a mismatch is detectable in tests.
fn hash(n: u8) -> BlockHash {
    BlockHash::from([n; 32])
}

fn height(n: u32) -> Height {
    Height::try_from(n).expect("test heights are in range")
}

#[test]
fn an_empty_seam_has_neither_quantity() {
    let (horizon, watermark) = Seam::new(100, 10).split();
    assert_eq!(horizon.durable(), None);
    assert_eq!(horizon.retention_floor(), None);
    assert_eq!(watermark.released(), None);
}

#[test]
fn the_horizon_is_the_tip_less_the_reorg_depth() {
    let (mut horizon, watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("first publish is legal");
    assert_eq!(released.height(), height(900));
    assert_eq!(released.hash(), hash(1));
    assert_eq!(watermark.released().map(|r| r.height()), Some(height(900)));
}

#[test]
fn a_tip_below_the_reorg_depth_saturates_at_genesis() {
    let (mut horizon, _watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(30), hash(1)).expect("saturating publish is legal");
    assert_eq!(released.height(), Height::GENESIS);
}

#[test]
fn the_watermark_cannot_advance_without_an_authorisation() {
    // Review Focus 1: no `Released` exists yet, so there is nothing to present.
    let (_horizon, watermark) = Seam::new(100, 10).split();
    assert_eq!(watermark.released(), None);
}

#[test]
fn the_watermark_cannot_pass_the_horizon() {
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("legal");
    assert_eq!(
        watermark.advance(&released, height(901)),
        Err(SeamFault::WatermarkPastHorizon { to: height(901), horizon: height(900) })
    );
    // Review Focus 3: the rejection left the state untouched.
    assert_eq!(horizon.durable(), None);
}

#[test]
fn the_watermark_cannot_regress() {
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("legal");
    watermark.advance(&released, height(800)).expect("legal");
    assert_eq!(
        watermark.advance(&released, height(799)),
        Err(SeamFault::RegressedWatermark { to: height(799), held: height(800) })
    );
    assert_eq!(horizon.durable().map(|c| c.height()), Some(height(800)));
}

#[test]
fn the_horizon_cannot_regress() {
    let (mut horizon, _watermark) = Seam::new(100, 10).split();
    horizon.advance(height(1000), hash(1)).expect("legal");
    assert_eq!(
        horizon.advance(height(999), hash(2)),
        Err(SeamFault::RegressedHorizon { to: height(899), held: height(900) })
    );
}

#[test]
fn republishing_the_same_height_is_legal_and_idempotent() {
    // Review Focus 4: monotonicity is non-decreasing, so equality is not a fault.
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("legal");
    horizon.advance(height(1000), hash(1)).expect("equal republish is legal");
    watermark.advance(&released, height(800)).expect("legal");
    watermark.advance(&released, height(800)).expect("equal republish is legal");
    assert_eq!(horizon.durable().map(|c| c.height()), Some(height(800)));
}

#[test]
fn the_retention_floor_trails_the_watermark_by_the_margin() {
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    let released = horizon.advance(height(1000), hash(1)).expect("legal");
    watermark.advance(&released, height(800)).expect("legal");
    assert_eq!(horizon.retention_floor(), Some(height(790)));
}

#[test]
fn a_watermark_below_the_margin_saturates_the_floor_at_genesis() {
    // Review Focus 2: no underflow when the margin exceeds the watermark.
    let (mut horizon, mut watermark) = Seam::new(0, 10).split();
    let released = horizon.advance(height(3), hash(1)).expect("legal");
    watermark.advance(&released, height(3)).expect("legal");
    assert_eq!(horizon.retention_floor(), Some(Height::GENESIS));
}

#[test]
fn the_inequality_chain_holds_across_a_ratchet_run() {
    let (mut horizon, mut watermark) = Seam::new(100, 10).split();
    for tip in [1000_u32, 1100, 1200, 1300] {
        let released = horizon.advance(height(tip), hash(1)).expect("legal");
        watermark.advance(&released, released.height()).expect("following the horizon is legal");

        let floor = horizon.retention_floor().expect("a watermark exists");
        let w = horizon.durable().expect("a watermark exists").height();
        let r = released.height();
        assert!(u32::from(floor) + 10 <= u32::from(w), "floor + margin <= w");
        assert!(u32::from(w) <= u32::from(r), "w <= r");
        assert!(u32::from(r) <= tip - 100, "r <= t - d");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo nextest run -p zaino-finality`
Expected: FAIL — the crate has no `lib.rs` yet, so compilation fails with unresolved `crate::Seam`.

- [ ] **Step 4: Write the fault type**

`packages/zaino-finality/src/fault.rs`:

```rust
//! The faults, one per violated link in the seam's inequality chain.

use thiserror::Error;
use zaino_primitives::types::Height;

/// A publish that would break the seam's invariant. A fault leaves the seam's
/// state unchanged, so the next legal publish is validated against the same
/// baseline as the rejected one.
///
/// There is no variant for `r <= t - d`: the seam owns the reorg depth and
/// derives `r` from the tip its caller supplies, so a horizon inside the reorg
/// window is unrepresentable rather than rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SeamFault {
    /// The volatile tier published a horizon below the one it already holds,
    /// which needs a reorg deeper than the consensus bound.
    #[error("horizon regressed to {to:?} from {held:?}")]
    RegressedHorizon {
        /// The rejected horizon.
        to: Height,
        /// The horizon still held.
        held: Height,
    },
    /// The durable tier published a watermark below the one it already holds.
    /// No rewind path exists, so this is corruption rather than a rollback.
    #[error("watermark regressed to {to:?} from {held:?}")]
    RegressedWatermark {
        /// The rejected watermark.
        to: Height,
        /// The watermark still held.
        held: Height,
    },
    /// The durable tier committed past the horizon, writing volatile heights
    /// into an append-only store.
    #[error("watermark {to:?} is past the horizon {horizon:?}")]
    WatermarkPastHorizon {
        /// The rejected watermark.
        to: Height,
        /// The horizon it exceeded.
        horizon: Height,
    },
}
```

- [ ] **Step 5: Write the seam**

`packages/zaino-finality/src/seam.rs`:

```rust
//! The seam and its two halves.

use std::sync::Arc;

use tokio::sync::watch;
use zaino_primitives::types::{BlockHash, Height};

use crate::fault::SeamFault;

/// The reorg horizon: heights at or below this are past the reorg window and
/// the durable tier may commit them.
///
/// Issued only by [`ReorgHorizon::advance`] — it has no public constructor, so
/// a holder cannot fabricate an authorisation it was not given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Released {
    height: Height,
    hash: BlockHash,
}

impl Released {
    /// The highest height past the reorg window.
    pub fn height(&self) -> Height {
        self.height
    }

    /// The canonical hash at that height, backed by the volatile tier's
    /// parent-linked chain down from its tip.
    pub fn hash(&self) -> BlockHash {
        self.hash
    }
}

/// The durable watermark: heights at or below this are committed to disk.
///
/// Issued only by [`DurableWatermark::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    height: Height,
}

impl Committed {
    /// The highest height committed to disk.
    pub fn height(&self) -> Height {
        self.height
    }
}

/// The depths the seam owns, read once at construction.
#[derive(Debug, Clone, Copy)]
struct Depths {
    reorg: u32,
    retention_margin: u32,
}

/// The two legs of the ratchet, shared by both halves.
struct SeamState {
    depths: Depths,
    released: watch::Sender<Option<Released>>,
    committed: watch::Sender<Option<Committed>>,
}

/// The seam between the durable and volatile tiers.
///
/// Constructed once, before either tier, and immediately split: the channels
/// exist before the components, so neither tier has to be built before the
/// other. It is the only owner of the reorg depth and the retention margin.
pub struct Seam {
    state: Arc<SeamState>,
}

impl Seam {
    /// A seam over `reorg_depth` (the consensus reorg bound) and
    /// `retention_margin` (how far below the watermark the volatile tier keeps
    /// retaining, so the two tiers' ranges overlap and cannot gap).
    pub fn new(reorg_depth: u32, retention_margin: u32) -> Self {
        let (released, _) = watch::channel(None);
        let (committed, _) = watch::channel(None);
        Self {
            state: Arc::new(SeamState {
                depths: Depths {
                    reorg: reorg_depth,
                    retention_margin,
                },
                released,
                committed,
            }),
        }
    }

    /// The two halves: one per tier, each owning the quantity it publishes.
    pub fn split(self) -> (ReorgHorizon, DurableWatermark) {
        let committed_rx = self.state.committed.subscribe();
        let released_rx = self.state.released.subscribe();
        (
            ReorgHorizon {
                state: Arc::clone(&self.state),
                committed: committed_rx,
            },
            DurableWatermark {
                state: self.state,
                released: released_rx,
            },
        )
    }
}

/// The volatile tier's half. Owns the reorg horizon, reads the watermark.
///
/// Not `Clone`: a second holder would be a second publisher of the horizon,
/// which is the drift the seam exists to prevent.
pub struct ReorgHorizon {
    state: Arc<SeamState>,
    committed: watch::Receiver<Option<Committed>>,
}

impl ReorgHorizon {
    /// The consensus reorg depth, so the caller knows which height's hash to
    /// supply to [`advance`](Self::advance).
    pub fn reorg_depth(&self) -> u32 {
        self.state.depths.reorg
    }

    /// Publishes the horizon derived from `tip`.
    ///
    /// The caller supplies its verified tip and the canonical hash of the block
    /// `reorg_depth` below it, never the horizon itself: the seam applies the
    /// derivation, so a horizon inside the reorg window cannot be named.
    pub fn advance(
        &mut self,
        tip: Height,
        hash_at_horizon: BlockHash,
    ) -> Result<Released, SeamFault> {
        let to = tip.saturating_sub(self.state.depths.reorg);
        if let Some(held) = *self.state.released.borrow() {
            if u32::from(to) < u32::from(held.height) {
                return Err(SeamFault::RegressedHorizon {
                    to,
                    held: held.height,
                });
            }
        }
        let released = Released {
            height: to,
            hash: hash_at_horizon,
        };
        self.state.released.send_replace(Some(released));
        Ok(released)
    }

    /// The durable tier's watermark, or `None` while it holds nothing.
    pub fn durable(&self) -> Option<Committed> {
        *self.committed.borrow()
    }

    /// The lowest height this tier must keep retaining: the watermark less the
    /// retention margin, saturating at genesis. `None` while the durable tier
    /// holds nothing, when it must retain everything it has.
    ///
    /// Computed here so the margin has exactly one reader.
    pub fn retention_floor(&self) -> Option<Height> {
        self.durable()
            .map(|committed| committed.height.saturating_sub(self.state.depths.retention_margin))
    }
}

/// The durable tier's half. Owns the watermark, reads the horizon.
///
/// Not `Clone`, for the same reason as [`ReorgHorizon`].
pub struct DurableWatermark {
    state: Arc<SeamState>,
    released: watch::Receiver<Option<Released>>,
}

impl DurableWatermark {
    /// The current horizon, or `None` while the volatile tier has published none.
    pub fn released(&self) -> Option<Released> {
        *self.released.borrow()
    }

    /// Resolves on the next horizon the volatile tier publishes.
    ///
    /// A `watch`, so a caller that falls behind skips to the present rather than
    /// replaying every intermediate horizon.
    pub async fn await_released(&mut self) -> Released {
        loop {
            if self.released.changed().await.is_err() {
                // The seam outlives both halves (both hold an Arc), so the
                // sender cannot drop while this receiver lives.
                unreachable!("the seam state outlives this half");
            }
            if let Some(released) = *self.released.borrow_and_update() {
                return released;
            }
        }
    }

    /// Publishes the watermark reached under `authorised_by`.
    ///
    /// Requires the horizon that authorised the work, so advancing on this
    /// tier's own authority is unrepresentable rather than merely rejected.
    pub fn advance(
        &mut self,
        authorised_by: &Released,
        to: Height,
    ) -> Result<Committed, SeamFault> {
        if u32::from(to) > u32::from(authorised_by.height) {
            return Err(SeamFault::WatermarkPastHorizon {
                to,
                horizon: authorised_by.height,
            });
        }
        if let Some(held) = *self.state.committed.borrow() {
            if u32::from(to) < u32::from(held.height) {
                return Err(SeamFault::RegressedWatermark {
                    to,
                    held: held.height,
                });
            }
        }
        let committed = Committed { height: to };
        self.state.committed.send_replace(Some(committed));
        Ok(committed)
    }
}
```

- [ ] **Step 6: Write the crate root**

`packages/zaino-finality/src/lib.rs`:

```rust
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
pub use seam::{Committed, DurableWatermark, ReorgHorizon, Released, Seam};
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo nextest run -p zaino-finality`
Expected: PASS, 11 tests.

- [ ] **Step 8: Lint**

Run: `makers clippy && makers doc && makers fmt`
Expected: clean. Fix any `missing_docs` warnings on public items.

- [ ] **Step 9: Write the changeset and usage.md**

Run: `relman changeset new`, then edit the created file to:

```toml
[[changes]]
crate = "zaino-finality"
kind = "added"
section = "Added"
description = "New crate. `Seam` owns the reorg depth, the retention margin, and both legs of the finalised/non-finalised ratchet, splitting into a `ReorgHorizon` half for the volatile tier and a `DurableWatermark` half for the durable one. `Released` is unforgeable and gates `DurableWatermark::advance`."
```

`packages/zaino-finality/usage.md` states the inequality chain, the ownership table, and that `zaino-runtime` is the only crate that constructs a seam.

- [ ] **Step 10: Commit**

```bash
git add packages/zaino-finality Cargo.toml .changesets
git commit -m "feat(finality): the seam contract as one owner per quantity"
```

---

### Task 2: The chain-head service publishes the horizon

**Files:**
- Modify: `packages/zaino-chain-head-service/src/service.rs` (the `confirmed_watermark` field, `anchor`/`anchored` signatures, `RETENTION_MARGIN`, the trim site)
- Modify: `packages/zaino-chain-head-service/Cargo.toml` (add `zaino-finality`)
- Modify: `packages/zaino-chain-head-service/src/tests.rs` (construct a seam half)
- Create: `.changesets/finality-seam-chain-head.toml`

**Interfaces:**
- Consumes: `zaino_finality::{ReorgHorizon, Seam}` from Task 1.
- Produces: `ChainHeadService::anchor(source: Arc<S>, config: ChainHeadConfig, horizon: ReorgHorizon) -> Result<(ChainHeadSubscriber, Self), ChainHeadInitError>` — the third parameter changes type from `watch::Receiver<Option<Height>>`.

- [ ] **Step 1: Write the failing test**

Append to `packages/zaino-chain-head-service/src/tests.rs`:

```rust
#[tokio::test]
async fn advancing_publishes_the_horizon_and_trims_to_the_retention_floor() {
    let (horizon, mut watermark) = Seam::new(MAX_BLOCK_REORG_HEIGHT, 10).split();
    let source = OfflineValidator::with_chain_to(height(1000));
    let service = ChainHeadService::anchored(Arc::new(source), config(), horizon)
        .await
        .expect("the offline validator anchors");

    service.advance_once().await.expect("advancing succeeds");

    // The horizon reached the seam, derived from the graph's tip.
    let released = watermark.released().expect("the service published a horizon");
    assert_eq!(
        u32::from(released.height()),
        1000 - MAX_BLOCK_REORG_HEIGHT,
        "r = t - d"
    );

    // With no watermark published, the floor is None, so nothing below the
    // reorg-safety floor is dropped on the durable tier's account.
    let snapshot = service.subscriber().snapshot();
    assert_eq!(
        u32::from(snapshot.coverage().expect("a window").start),
        1000 - MAX_BLOCK_REORG_HEIGHT,
        "the reorg-safety floor alone bounds retention while w is None"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p zaino-chain-head-service advancing_publishes_the_horizon`
Expected: FAIL — `anchored` takes a `watch::Receiver`, so the call does not typecheck.

- [ ] **Step 3: Swap the field and the constructors**

In `packages/zaino-chain-head-service/Cargo.toml` add to `[dependencies]`:

```toml
zaino-finality = { workspace = true }
```

In `service.rs`, delete the `RETENTION_MARGIN` constant (it moves to `Seam::new`'s caller) and replace the field:

```rust
    /// The seam's volatile half: publishes this tier's reorg horizon and reads
    /// the durable tier's watermark. Read at trim time so the floor never rises
    /// above what the durable tier can already serve.
    horizon: ReorgHorizon,
```

Change `anchor` and `anchored` to take `horizon: ReorgHorizon` in place of
`confirmed_watermark: watch::Receiver<Option<Height>>`, threading it into the
struct unchanged.

- [ ] **Step 4: Publish the horizon and read the floor**

At the trim site in the advance path, replace the two-floor computation:

```rust
        // Trim floor: retain every height at or above it, drop below. Two
        // independent floors are computed and the lower — the one that retains
        // more — wins:
        //
        // - the reorg-safety floor keeps the whole consensus reorg window, so a
        //   reorg can always be walked back to its fork point regardless of what
        //   the durable tier has confirmed;
        // - the seam's retention floor keeps everything the durable tier has not
        //   yet committed, less the overlap. `None` while that tier holds
        //   nothing, when nothing below the reorg window is dropped on its
        //   account.
        let reorg_safety_floor = height_below(graph.best_tip().height, self.config.max_depth());
        let floor = match self.horizon.retention_floor() {
            Some(retention_floor) => reorg_safety_floor.min(retention_floor),
            None => Height::GENESIS,
        };
        graph.remove_finalised_blocks(floor);

        // Publish this tier's horizon: the seam derives `r` from the tip, so
        // this supplies the verified tip and the canonical hash `d` below it.
        let tip = graph.best_tip().height;
        let horizon_height = height_below(tip, self.horizon.reorg_depth());
        // `best_block_by_height` is the graph's canonical-chain lookup (there is
        // no `block_hash`); the graph's gapless-canonical invariant means this is
        // `Some` for every height from its floor to its tip.
        if let Some(hash) = graph.best_block_by_height(horizon_height).map(|block| block.hash) {
            match self.horizon.advance(tip, hash) {
                Ok(_released) => {}
                Err(fault) => {
                    // A seam fault is a breach of an invariant this tier owns,
                    // not a transient condition: report it rather than retrying
                    // a publish that will be rejected identically.
                    warn!(%fault, "the chain head's horizon publish breached the seam invariant");
                }
            }
        }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p zaino-chain-head-service`
Expected: PASS. Existing tests that constructed a `watch::channel` now construct `Seam::new(MAX_BLOCK_REORG_HEIGHT, 10).split()` and keep the `DurableWatermark` half alive for the test's duration.

- [ ] **Step 6: Commit**

```bash
makers clippy && makers fmt
relman changeset new   # kind = "breaking": anchor's third parameter changed type
git add packages/zaino-chain-head-service .changesets
git commit -m "refactor(chain-head)!: publish the reorg horizon through the seam"
```

---

### Task 3: The indexer consumes the horizon

**Files:**
- Modify: `packages/zaino-indexer/src/source_provisioner.rs` (`SyncTuning`, `SourceSyncDriver` field, `finalised`, the run loop, the progress poller)
- Modify: `packages/zaino-indexer/Cargo.toml` (add `zaino-finality`)
- Modify: `packages/zaino-sync/src/engine.rs` (the publish site at line 549)
- Modify: `packages/zaino-sync/Cargo.toml` (add `zaino-finality`)
- Create: `.changesets/finality-seam-indexer.toml`

**Interfaces:**
- Consumes: `zaino_finality::{DurableWatermark, Released, SeamFault}` from Task 1.
- Produces: `SyncTarget` enum with variants `Seam(DurableWatermark)` and `Depth { depth: u32 }`; `SourceSyncDriver::resuming`/`resuming_compact` take `target: SyncTarget` in place of reading `tuning.finalised_depth`; `SyncEngine::new` takes `watermark: Option<DurableWatermark>`.

- [ ] **Step 0: Create the test module and its helpers**

`zaino-indexer` has **no tests today** (`grep '#\[cfg(test)\]' packages/zaino-indexer/src/` is
empty), so the module and its helpers do not exist yet. Create
`packages/zaino-indexer/src/tests.rs`, declare `#[cfg(test)] mod tests;` in
`lib.rs`, and add to `[dev-dependencies]` in `packages/zaino-indexer/Cargo.toml`:

```toml
zaino-source = { workspace = true, features = ["testing"] }
zaino-sync = { path = "../zaino-sync", features = ["testing"] }
```

The mock validator is `zaino_source::mock` (gated behind that crate's existing
`testing` feature); the in-memory backend is
`zaino_sync::testing::InMemoryBackend`. Write the helpers at the top of
`tests.rs`:

```rust
use zaino_primitives::types::{BlockHash, Height};

fn height(n: u32) -> Height {
    Height::try_from(n).expect("test heights are in range")
}

fn hash(n: u8) -> BlockHash {
    BlockHash::from([n; 32])
}

fn tuning() -> SyncTuning {
    SyncTuning {
        batch_size: 4,
        channel_capacity: 8,
        concurrency: FetchConcurrency::Serial,
    }
}

/// Polls the persisted watermark until it reaches `target`, so a test observes
/// durability rather than guessing at a delay.
async fn wait_for_watermark(backend: &InMemoryBackend, target: Height) {
    for _ in 0..400 {
        let committed = backend
            .reader()
            .ok()
            .and_then(|reader| zaino_persistence_codec::watermark::read(&reader).ok().flatten());
        if committed == Some(target) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the watermark never reached {target:?}");
}
```

`MockSource::with_chain_to(tip)` and `MockSource::with_hash_at(height, hash)` are
thin constructors over `zaino_source::mock` — add them there if the mock does not
already offer a way to fix the hash served at one height, since Review Focus 5
needs a source that disagrees with the seam at exactly the horizon.

- [ ] **Step 1: Write the failing tests**

In `packages/zaino-indexer/src/tests.rs`:

```rust
#[tokio::test]
async fn the_driver_refuses_a_block_whose_hash_differs_from_the_horizon() {
    // Review Focus 5: a branch disagreement at the seam must be refused before
    // anything is written, not detected after divergent data is committed.
    let (mut horizon, watermark) = Seam::new(0, 10).split();
    let released = horizon
        .advance(height(10), hash(0xAA))
        .expect("first publish is legal");
    assert_eq!(released.height(), height(10));

    // The source serves a different block at height 10 than the horizon names.
    let source = Arc::new(MockSource::with_hash_at(height(10), hash(0xBB)));
    let backend = InMemoryBackend::default();
    let driver = SourceSyncDriver::resuming(
        &backend,
        CurrentZaino::pipelines(),
        Arc::clone(&source),
        |block| context_from_block(&block),
        tuning(),
        SyncTarget::Seam(watermark),
    )
    .expect("the driver builds");

    let error = driver
        .run(reporter(), CancellationToken::new())
        .await
        .expect_err("a branch disagreement at the horizon is refused");
    assert!(
        matches!(error, IndexerError::HorizonBranchMismatch { .. }),
        "got {error:?}"
    );
    assert_eq!(
        zaino_persistence_codec::watermark::read(&backend.reader().expect("reader"))
            .expect("read"),
        None,
        "nothing was committed"
    );
}

#[tokio::test]
async fn the_driver_indexes_to_the_published_horizon() {
    let (mut horizon, watermark) = Seam::new(0, 10).split();
    horizon.advance(height(10), hash(10)).expect("legal");

    let source = Arc::new(MockSource::with_chain_to(height(10)));
    let backend = InMemoryBackend::default();
    let driver = SourceSyncDriver::resuming(
        &backend,
        CurrentZaino::pipelines(),
        Arc::clone(&source),
        |block| context_from_block(&block),
        tuning(),
        SyncTarget::Seam(watermark),
    )
    .expect("the driver builds");

    let cancel = CancellationToken::new();
    let run = tokio::spawn({
        let cancel = cancel.clone();
        async move { driver.run(reporter(), cancel).await }
    });

    // The driver reaches the horizon and no further.
    wait_for_watermark(&backend, height(10)).await;
    cancel.cancel();
    run.await.expect("the task joins").expect("the run ends cleanly");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p zaino-indexer the_driver_`
Expected: FAIL — `SyncTarget` and `IndexerError::HorizonBranchMismatch` do not exist.

- [ ] **Step 3: Add the target abstraction and the fault**

In `packages/zaino-indexer/Cargo.toml` add `zaino-finality = { workspace = true }`.

In `source_provisioner.rs`, delete `finalised_depth` from `SyncTuning` and add:

```rust
/// Where the driver's sync target comes from.
///
/// In a composed runtime the seam has one owner, so the driver *consumes* the
/// horizon. Standalone — an isolated finalised store, or a benchmark over a
/// non-reorging source — there is no volatile tier to own it, so the depth
/// derivation is honest there and only there.
pub enum SyncTarget {
    /// The composed runtime: the target is the horizon the volatile tier
    /// publishes, and the watermark is published back through the same half.
    Seam(DurableWatermark),
    /// Standalone: `tip - depth`, with no volatile tier to coordinate with.
    /// Pass `zaino_consensus::MAX_BLOCK_REORG_HEIGHT`, or `0` over a
    /// non-reorging test source.
    Depth {
        /// Depth below the tip treated as still volatile.
        depth: u32,
    },
}
```

Add to `IndexerError`:

```rust
    /// The source served a different block at the horizon than the volatile
    /// tier named. Refused before anything is written: committing it would put
    /// a branch the volatile tier does not hold into an append-only store.
    #[error("source served {served:?} at horizon height {height:?}, but the seam named {named:?}")]
    HorizonBranchMismatch {
        /// The horizon's height.
        height: Height,
        /// The hash the seam named.
        named: BlockHash,
        /// The hash the source served.
        served: BlockHash,
    },
```

- [ ] **Step 4: Drive the loop from the target**

Replace `fn finalised(&self, tip: Height) -> Height` with a method on the target
that yields both the height and the authorisation, and replace the run loop's
`tips.changed()` arm for the `Seam` case:

```rust
impl SyncTarget {
    /// The next target to build to, and the authorisation for it when the seam
    /// owns the boundary.
    async fn next(&mut self, tips: &mut watch::Receiver<ChainTip>) -> Option<(Height, Option<Released>)> {
        match self {
            // The horizon drives the loop: the volatile tier publishes it when
            // its own tip advances, so the driver needs no tip subscription of
            // its own to know when to build.
            Self::Seam(watermark) => {
                let released = watermark.await_released().await;
                Some((released.height(), Some(released)))
            }
            Self::Depth { depth } => {
                tips.changed().await.ok()?;
                let tip = tips.borrow_and_update().height;
                Some((tip.saturating_sub(*depth), None))
            }
        }
    }
}
```

In the initial catch-up, read the current value rather than awaiting:

```rust
        let (mut synced, authorisation) = match &self.target {
            SyncTarget::Seam(watermark) => match watermark.released() {
                Some(released) => (released.height(), Some(released)),
                // The volatile tier has not anchored yet. It will publish on
                // its first advance, which the follow loop below awaits.
                None => (self.start, None),
            },
            SyncTarget::Depth { depth } => {
                let tip = self.provisioner.current_tip().await?;
                (tip.saturating_sub(*depth), None)
            }
        };
```

- [ ] **Step 5: Check the horizon's hash before writing**

In `sync_to`, before handing the provisioned range to the engine, verify the
item at the target height against the authorisation:

```rust
    /// Refuses a range whose top block is not the one the seam named.
    ///
    /// Checked here, where the fetched block is in hand, rather than at publish
    /// time: the engine knows when a batch became durable but not which block
    /// it was, and a mismatch must stop the write rather than be reported after
    /// divergent data is on disk.
    fn check_horizon_branch(
        authorised_by: Option<&Released>,
        at: Height,
        served: BlockHash,
    ) -> Result<(), IndexerError> {
        let Some(released) = authorised_by else {
            return Ok(());
        };
        if at != released.height() {
            return Ok(());
        }
        if served == released.hash() {
            return Ok(());
        }
        Err(IndexerError::HorizonBranchMismatch {
            height: at,
            named: released.hash(),
            served,
        })
    }
```

- [ ] **Step 6: Publish the watermark through the half**

In `packages/zaino-sync/Cargo.toml` add `zaino-finality = { workspace = true }`.
In `engine.rs`, replace the `watch::Sender<Option<Height>>` field with **two**
fields — the half, and the authorisation the current range is being built under:

```rust
    /// The seam's durable half, when composed. `None` standalone, where there is
    /// no volatile tier to coordinate with.
    watermark: Option<DurableWatermark>,
    /// The horizon the range in flight was authorised by, set by the driver
    /// before each `sync_to` and cleared when it returns. The engine publishes
    /// with it, so the authorisation travels with the work rather than being
    /// re-read at publish time.
    authorisation: Option<Released>,
```

Add `pub fn set_authorisation(&mut self, authorised_by: Option<Released>)` so the
driver sets it before handing a range over. Then replace the publish at line 549
with:

```rust
            // The batch — including the watermark stamp — is now durable, so the
            // watermark can be published: it never leads its data. The
            // authorisation is the horizon the driver built this range under, so
            // the seam refuses a watermark past it.
            if let (Some(watermark), Some(authorised_by)) =
                (self.watermark.as_mut(), self.authorisation.as_ref())
            {
                if let Err(fault) = watermark.advance(authorised_by, watermark_height) {
                    return Err(SyncError::Seam(fault));
                }
            }
```

Add `#[error("the seam refused the committed watermark")] Seam(#[source] SeamFault)` to `SyncError`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo nextest run -p zaino-indexer -p zaino-sync`
Expected: PASS. Call sites passing `finalised_depth` in `SyncTuning` now pass `SyncTarget::Depth { depth }`.

- [ ] **Step 8: Commit**

```bash
makers clippy && makers fmt
relman changeset new   # two entries: zaino-indexer (breaking), zaino-sync (breaking)
git add packages/zaino-indexer packages/zaino-sync .changesets
git commit -m "refactor(indexer)!: consume the horizon instead of deriving the boundary"
```

---

### Task 4: Boot constructs the seam

**Files:**
- Modify: `packages/zaino-runtime/src/boot.rs` (the `deploy` and `assemble` wiring)
- Modify: `packages/zaino-runtime/src/deployment.rs` (`IndexedDeploymentConfig`: drop `finalised_depth`)
- Modify: `packages/zaino-runtime/Cargo.toml` (add `zaino-finality`)
- Modify: `docs/example_configs/zainod.toml` (drop the removed key)
- Create: `.changesets/finality-seam-runtime.toml`

**Interfaces:**
- Consumes: `Seam` from Task 1, `ChainHeadService::anchor(.., ReorgHorizon)` from Task 2, `SyncTarget` from Task 3.
- Produces: no new public API; `IndexedDeploymentConfig` loses its `indexer.finalised_depth` field.

- [ ] **Step 1: Write the failing test**

Add to `packages/zaino-runtime/src/boot.rs`'s test module:

```rust
#[tokio::test]
async fn boot_wires_one_seam_through_both_tiers() {
    // The ratchet turns end to end: the chain head publishes a horizon, the
    // indexer builds to it and publishes a watermark, and the chain head's
    // retention floor follows. Asserted through the seam rather than by
    // inspecting either component.
    // Both halves are moved into their components, so the assertion goes
    // through what each tier already exposes: the store's persisted watermark
    // and the chain head's published coverage.
    let (orchestra, backend, chain_head) =
        deploy_for_test::<TestDeployment>(test_config(), offline_source()).await;

    wait_for_watermark(&backend, height(10)).await;
    let w = zaino_persistence_codec::watermark::read(&backend.reader().expect("reader"))
        .expect("read")
        .expect("a watermark");
    let nfs_floor = chain_head.snapshot().coverage().expect("a window").start;
    assert!(
        u32::from(nfs_floor) <= u32::from(w),
        "floor <= w: the ratchet turned and the tiers overlap"
    );
    orchestra.shutdown().await;
}
```

`deploy_for_test` is a thin wrapper this task adds beside `deploy`, returning the
backend handle and the `ChainHeadSubscriber` alongside the `Orchestra` so a test
can observe both tiers. `Orchestra` exposes neither today, and widening its
public API for one test would be the wrong trade.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p zaino-runtime boot_wires_one_seam`
Expected: FAIL — `deploy` still builds the driver from `tuning.finalised_depth`.

- [ ] **Step 3: Construct and split the seam in `deploy`**

In `boot.rs`, before building the driver:

```rust
    // The seam is constructed before either tier and split immediately: the
    // channels predate the components, so neither has to exist before the
    // other. This is the only place a seam is built, and the only place the
    // reorg depth and the retention overlap are read.
    let (horizon, watermark) = Seam::new(MAX_BLOCK_REORG_HEIGHT, RETENTION_MARGIN).split();
```

Pass `SyncTarget::Seam(watermark)` to `resuming`/`resuming_compact`, and `horizon`
to `ChainHeadService::anchor` in `assemble` in place of
`driver.subscribe_confirmed_watermark()`. Delete that call and the comment above
it. Thread `horizon` into `assemble` as a parameter.

- [ ] **Step 4: Drop the config key**

Remove `finalised_depth` from `IndexedDeploymentConfig`'s indexer section and
from `docs/example_configs/zainod.toml`. Declare `RETENTION_MARGIN: u32 = 10` in
`boot.rs` with the overlap rationale, since the chain-head service no longer
owns it.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p zaino-runtime`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
makers clippy && makers fmt && makers doc
relman changeset new   # kind = "breaking": the indexer.finalised_depth config key is gone
git add packages/zaino-runtime docs/example_configs .changesets
git commit -m "refactor(runtime)!: construct the one seam and drop finalised_depth"
```

---

### Task 5: `seam_run` exercises the real ratchet

**Files:**
- Modify: `packages/zaino-core/examples/seam_run.rs` (the dead watermark channel at line 286, `finalised_depth: 0` at line 217)
- Modify: `packages/zaino-core/Cargo.toml` (add `zaino-finality` as a dev-dependency)

**Interfaces:**
- Consumes: `Seam` from Task 1, the changed `anchor` from Task 2, `SyncTarget` from Task 3.
- Produces: nothing; it is an example.

- [ ] **Step 1: Replace the faked legs with one seam**

The example currently disables both legs to stay deterministic — a watermark
channel never published to, and a zero finalised depth. Replace both:

```rust
    // One seam, as the runtime builds it. A zero reorg depth keeps the walk
    // deterministic (the horizon is the tip), while the retention margin stays
    // real so the overlap the seam guarantees is what the assertions below see.
    let (horizon, watermark) = Seam::new(0, RETENTION_MARGIN).split();
```

Pass `SyncTarget::Seam(watermark)` where `SyncTuning { finalised_depth: 0, .. }`
was, and `horizon` to `ChainHeadService::anchored` in place of the
`watch::channel` pair. Delete the channel and the comment explaining why it was
inert.

- [ ] **Step 2: Assert the chain across the walk**

After the existing region assertions, add:

```rust
    // The seam's invariant, observed through the composed snapshot rather than
    // through either tier: the durable range ends at or below the horizon, and
    // the volatile range starts at or below the durable end, so the union has
    // no gap.
    let serviceable = snapshot.serviceable_range().expect("a served range");
    let fs_end = serviceable.watermark.expect("a watermark");
    let nfs_start = nfs_coverage.start;
    assert!(
        u32::from(nfs_start) <= u32::from(fs_end),
        "floor <= w: the tiers overlap, so the union has no gap"
    );
```

- [ ] **Step 3: Run the example**

Run: `cargo run -p zaino-core --example seam_run`
Expected: exits 0, and its self-verifying `matches!` assertions still pass for
each seam region. The initial-build gap it demonstrates is unchanged — that gap
is a cold-start property, not a seam fault.

- [ ] **Step 4: Commit**

```bash
makers clippy && makers fmt
git add packages/zaino-core
git commit -m "test(core): drive seam_run through the real ratchet"
```

---

## Spec amendments this plan makes

Two lines of `docs/notes/finality-seam.md` are superseded; the spec should be
updated to match before the branch merges.

**`SeamFault::BranchMismatch` becomes a driver-side precondition**, not a seam
variant. See Deferred below.

**No port traits and no `StubSeam`.** The spec's testing section says components
become generic over their half, admitting a stub behind a `testing` feature. That
predates the single-crate decision: with `Seam` and both halves in one crate that
every consumer already depends on, a real seam is constructible in a test with
`Seam::new(d, margin).split()` — which is what every test in this plan does. A
stub would be a second implementation of a type that is already cheap to build,
so the halves stay concrete and no port traits are introduced.

## Deferred

**`Released.hash` is checked at fetch time, not at publish time.** The check
lives in the driver (Task 3, Step 5) because the engine knows when a batch
became durable but not which block it was, and `SyncEngine<Ctx, B>` is generic
over a context type that exposes no hash. Moving the check into the seam would
mean a new bound on `Ctx`, which ripples to every context and test context in
`zaino-sync` and `zaino-indexes` — a cross-cutting change that deserves its own
review rather than being folded in here. The fetch-time check is strictly
earlier than a publish-time one, so nothing is lost on the safety axis; what is
deferred is `SeamFault::BranchMismatch` as a seam-level variant.
