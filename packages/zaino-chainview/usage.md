# zaino-chainview

Composes the **finalised store** (FS — the sync-engine index, via `zaino-store`'s
`StoreReader`/`CompactBlockRead`) and a **non-finalised view** (NFS — the volatile
chain-head) into **one served snapshot** for compact-block serving.

## The seam

A read for height `h` routes on the FS **watermark** (`w = fs.pinned_tip().height`,
`None` when the FS is empty):

- `h ≤ w` → **FS** (durable, finalised).
- `w < h ≤ nfs.tip` and `h ≥ nfs.floor` → **NFS** (volatile window).
- `h > nfs.tip` → `None` (no such block).
- otherwise (a height on-chain that *neither* side holds — the FS still building
  below the NFS floor, or the head not yet populated) → **`NotServiceable`**.

The watermark and the NFS view are captured **together** in one `snapshot()` so the
seam is coherent (idky's coherence rule).

## Invariants

- **An empty/lagging FS is normal.** `w == None` routes *everything* to the NFS —
  a young chain (fewer blocks than the finalisation depth) is served entirely from
  the non-finalised side. FS is the eventual deep-prefix optimisation, not a
  precondition.
- **The initial-build gap is an explicit policy knob.** A fresh FS still building
  from genesis on a mature chain leaves a middle (above `w`, below `nfs.floor`)
  that neither side holds. Stage 1 returns `NotServiceable` there — it does **not**
  source-fill. The watermark handshake (Stage 3) shrinks this gap to nothing *at
  the finalisation seam*; whether to source-fill the *initial-build* gap is a
  separate decision left open here.

## Thin marker

The composed snapshot impls only `Snapshot` (coherence: `pinned_tip` +
`serviceable_range`) and `CompactBlockRead` — the reads compact-block serving
needs. It does **not** force `TransactionRead`/`TreestateRead`/etc. onto consumers
(those are named separately, or passed through). This is deliberately thinner than
idky's forced-5-cap `ChainViewSnapshot` and nachog00's 9-method `ChainHeadSnapshot`.

## Stage

Stage 1 of the NFS greenfield: the FS⊕NFS **route**, with the real chain-head
stubbed (`testing::StubNonFinalised`). Stage 2 replaces the stub with a
`ChainGraph`-backed head; Stage 3 wires the confirm-before-trim watermark handshake.
