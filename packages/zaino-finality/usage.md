# `zaino-finality`

The single-owner contract for the seam between Zaino's durable tier (the
append-only finalised store) and its volatile one (the non-finalised chain
head). The boundary between the two tiers used to be derived independently by
each side from two different config constants, which could drift and silently
corrupt the append-only store. This crate makes that boundary have exactly one
owner.

## The inequality chain

The whole contract is one chain of inequalities, where `t` is the validator's
tip, `d` the consensus reorg depth, and `margin` the retention overlap:

```text
genesis  <=  floor + margin  <=  w  <=  r  <=  t - d  <=  t
```

- `r` is the **reorg horizon**: the highest height past the reorg window, derived
  as `r = height(t) - d`.
- `w` is the **durable watermark**: the highest height committed to disk.
- `floor` is the **retention floor**: the lowest height the volatile tier must
  keep retaining, `w - margin`, so the two tiers' served ranges overlap and
  their union cannot gap.

Gapless served coverage follows from `floor <= w`:

```text
[genesis, w] union [floor, t] = [genesis, t]
```

## Ownership

Each link has exactly one party permitted to move its left-hand side:

| Quantity | Owner (publisher) | Bound | Read by |
|----------|-------------------|-------|---------|
| `r` (reorg horizon)    | volatile tier (`ReorgHorizon`)   | `t - d`      | durable tier |
| `w` (durable watermark)| durable tier (`DurableWatermark`)| `r`          | volatile tier |
| `floor` (retention)    | volatile tier (`ReorgHorizon`)   | `w - margin` | — (internal) |

The seam carries exactly two quantities, `r` and `w`. `floor` is internal to the
volatile tier and never crosses the seam; it is computed from `w` by
`ReorgHorizon::retention_floor`, so the margin has exactly one reader.

`r` is derived, not chosen: the volatile tier supplies its verified tip and the
canonical hash `d` below it, and the seam applies the derivation. A horizon
inside the reorg window is therefore unrepresentable rather than rejected, and
there is no `SeamFault` for `r <= t - d`.

## Why the types

- Neither `ReorgHorizon` nor `DurableWatermark` is `Clone`, and both publish
  through `&mut self`: single-writer per quantity holds at the borrow checker as
  well as at the type level.
- *Reading* a quantity is not rationed — only *publishing* is. The durable tier
  holds the single `DurableWatermark`, but a quantity may have many readers, so
  `DurableWatermark::reader` hands out a `HorizonReader`: a cloneable, read-only
  view of the reorg horizon for parties that only observe it — the indexer's sync
  loop drives its boundary from one, and its progress poller reports against
  another — without holding the publishing capability. Observing the horizon
  cannot advance the watermark; that still requires the `DurableWatermark`
  itself.
- `Released` (the authorisation `ReorgHorizon::advance` issues) has no public
  constructor, and `DurableWatermark::advance` requires one, so the durable tier
  cannot advance on its own authority — the cross-tier relation `w <= r` is held
  in one place and applied on every publish.
- A rejected publish returns a `SeamFault` and leaves the seam's state
  unchanged, so the next legal publish is validated against the same baseline.

## Who constructs a seam

`zaino-runtime` is the only crate that constructs a `Seam`. It calls
`Seam::new(reorg_depth, retention_margin)` once, before either tier exists, and
immediately `split()`s it into the two halves, handing one to each tier. No
other crate constructs a seam; the tiers receive their half already split.
`DEFAULT_RETENTION_MARGIN` is exported for the construction site.
