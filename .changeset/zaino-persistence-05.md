---
default: major
---

# `zaino-persistence`: Two read contracts: `CommittedView` (`SequenceRead + MapRead + 'static`, a committed snapshot read on any …

Two read contracts: `CommittedView` (`SequenceRead + MapRead + 'static`, a committed snapshot read on any thread; `DiskView` and `OverlayView<V, Overlay>` over one) is what serves, and `Store::View: CommittedView`; `Store::staged` returns `StagedView<'_, V>` = `OverlayView<V, &WriteBuffer>`, a borrow of the store (holding one across `apply` no longer compiles, never a buffer copy). `View` drops its `'static` bound.

**Migration:** Bound served views on `V: CommittedView` (it was `SequenceRead + MapRead`); read a staged view inside the fold and drop it before `apply`.
