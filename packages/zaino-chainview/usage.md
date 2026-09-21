# zaino-chainview

Composes a **finalised store** (FS — the durable prefix) and a **non-finalised
head** (NFS — the volatile suffix) into **one served snapshot** for compact-block
serving.

Both sides are named only through the shared ports defined in `zaino-service`:
each is a `TakeSnapshot` whose snapshot is a `ChainSegment` (coherence
coordinate: `pinned_tip` + `coverage`) and a `CompactBlockRead` (compact-block
reads by height or hash). The composer assigns the roles by slot — `fs` is the
durable prefix, `nfs` the volatile suffix — so neither side describes its own
durability. `zaino-chainview` therefore depends only on `zaino-service`; it knows
nothing of the store or the chain-head crates.

The concrete segments live elsewhere: the FS segment is `zaino-store`'s
`StoreReader`/`StoreSnapshot`; the NFS segment adapter is in
`zaino-chain-head-service` (`ChainHeadSubscriber: TakeSnapshot`, its
`HeadSnapshot` a `ChainSegment + CompactBlockRead`). A production composition
(zainod) pairs the two.

## The seam

A read for height `h` routes on the FS **watermark** (`w = fs.coverage().end`,
`None` when the FS is empty) and the NFS **coverage** (`[floor, tip]`):

- `h ≤ w` → **FS** (durable, finalised).
- `floor ≤ h ≤ tip` → **NFS** (volatile window).
- `h > tip` → `None` (no such block).
- otherwise (a height on-chain that *neither* side holds — the FS still building
  below the NFS floor) → **`NotServiceable`**.

The FS and NFS snapshots are captured **together** in one `snapshot()` so the
seam is coherent: a read never mixes a watermark from one instant with a window
from another, and two reads of one pinned view cannot straddle a reorg. Both
sides pin an immutable view, so the capture is a cheap clone, not an I/O
round-trip.

Each NFS block's `ChainMetadata` comes from its own `TreeRoots`
(`ChainMetadata::from_tree_roots`), so the non-finalised side needs **no
cumulative index** to serve tree sizes — unlike the FS, which folds one.

## Invariants

- **An empty/lagging FS is normal.** `w == None` routes *everything* to the NFS —
  a young chain (fewer blocks than the finalisation depth) is served entirely from
  the non-finalised side. FS is the eventual deep-prefix optimisation, not a
  precondition.
- **The initial-build gap is an explicit policy knob.** A fresh FS still building
  from genesis on a mature chain leaves a middle (above `w`, below the NFS floor)
  that neither side holds. This stage returns `NotServiceable` there — it does
  **not** source-fill. A later watermark handshake shrinks this gap to nothing *at
  the finalisation seam*; whether to source-fill the *initial-build* gap is a
  separate decision left open here.

## Thin marker

The composed snapshot impls only `ChainSegment` (coherence: `pinned_tip` +
`coverage`), `Snapshot` (`serviceable_range`), and `CompactBlockRead` — the reads
compact-block serving needs. It does **not** force
`TransactionRead`/`TreestateRead`/etc. onto consumers (those are named
separately, or passed through).

## Testing

`testing::StubNonFinalised` (behind the `testing` feature) is an in-memory NFS
segment implementing the same `ChainSegment + CompactBlockRead + TakeSnapshot`
ports, so the FS⊕NFS route can be exercised without wiring the volatile graph.
