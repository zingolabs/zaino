---
default: major
---

# `checkadmission` through the source, balancer and chain view; peer pushes through the peer set

- `zaino-source`: `ChainDataSource::check_admission(bytes)` → `Result<TransactionId, QueryError<CheckAdmissionError>>` (`Refused(SendRawTransactionError)` | `Unsupported`). `MockValidator::checks_admission(bool)`, `Port::Check`, `Calls::checks`.
- `zaino-traffic`: `TrafficBalancer::check_admission(member, raw)`, a `Submit`-class ask of that member alone.
- `zaino-peers`: `PeerNetwork::push(raw)` (zebra's `PushTransaction` relay over the peer set) replaces `push_isolated(addr, raw)`; `entries(tip)` removed; `PushError` = `Wire` | `Peers(PeerError)`.
- `zaino-chainview`: `ValidatorP2pSource::push(raw)` replaces `push(entry, raw)`; `entries(tip)` removed. `MockPeers::pushes()` counts relays.

**Migration:** implement `check_admission` on any `ChainDataSource` (a fake can answer `Unsupported`); drop `entries` and the entry argument from `ValidatorP2pSource` implementations.
