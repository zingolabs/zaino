---
default: minor
---

# `zaino-traffic`: Feature `testing`: `MockPeerSet`, the peer set as a `PeerTransport` over one `MockChain` path

The `PeerSetViews` trait on `MockChain` gives `peer_set(tip)`: headers after the first locator hash held (`FindHeaders`, ≤ 160), blocks by hash, an unheld block refused as `Connection`; `hold(height)` moves the tip it holds.
