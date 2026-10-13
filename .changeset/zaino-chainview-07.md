---
default: major
---

# `zaino-chainview`: Header sync trusts its validators: no header is verified (no Equihash, no proof of work, no thread pool) and …

Header sync trusts its validators: no header is verified (no Equihash, no proof of work, no thread pool) and none is synced from genesis. With no chain, or a validator's claim − depth past the fetch ceiling, it anchors the header chain at that claim − depth; above the anchor it inserts each trusted header on its parent link and work. The chain is memory only: `header_sync(HeaderChain::new(depth))`, `HeaderSync::run` returns `()` and `HeaderStoreFailed` is gone.

**Migration:** Build the header chain with `HeaderChain::new(depth)` (no store, no `Params`) and spawn `header_sync.run(cancel)` as an infallible task.
