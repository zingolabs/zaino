---
default: major
---

# `zaino-chainview`: `Heard` removed; `PeerSource::heard` streams `zaino_primitives::types::Announced`

`Heard { peer, hash }` duplicated `zaino-peers`' `Announced`; both are now `zaino_primitives::types::Announced`, so a `PeerSource` over `zaino-peers` forwards its announcements unconverted.
