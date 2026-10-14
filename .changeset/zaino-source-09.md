---
default: minor
---

# `zaino-source`: what a validator reports about itself

New (from `zaino-primitives`): `BlockchainInfo` (`tip`, `estimated_height`, `activations`; `schedule()` = the activations as a `LocalNetwork`, `None` out of protocol order), `NodeRelease` and `EndOfService`, `PeerInfo`. `getblockchaininfo` parses only those fields: an upgrade this build does not know is skipped. Feature `testing`: `ValidatorViews::blockchain_info(tip)`.
