---
default: major
---

# `zaino-chainview`: `ChainViewSnapshot::validator_info()` removed; `EndpointSet::at` behind feature `testing`; …

`ChainViewSnapshot::validator_info()` removed (no consumer). `EndpointSet::at` is behind feature `testing`. `ChainView::new` returns `ConfigError::AddressCountMismatch { addresses, trusted }` when the addresses do not match the balancer's trusted members, instead of panicking.
