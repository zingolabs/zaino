---
default: minor
---

# `zaino-primitives`: `Announced { peer, hash }`, one peer's block `inv`

`types::Announced` is the one type for a block `inv` with its sender attached: `zaino-peers` emits it and `zaino-chainview`'s `PeerSource::heard` streams it.
