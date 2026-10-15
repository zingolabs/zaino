---
default: major
---

# `zaino-persistence`: `PinnedView` and `AppliedLayer` replace `DurableView` and `Uncommitted`

`DurableView` is now `PinnedView` (an NFS `OverlayView` holds in-memory rows, so the trait names what it guarantees: owned, fixed while held). `Uncommitted` is now `AppliedLayer`. "Durable" and "applied" name persistence states; "committed" is only the action. On-disk formats, metric names and `zainod verify` JSON keys are unchanged.

**Migration:** rename `DurableView` bounds to `PinnedView` and `Uncommitted` to `AppliedLayer`.
