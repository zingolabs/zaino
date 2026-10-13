---
default: minor
---

# `zaino-traffic`: The p2p network's peer set is one member (`MemberId::Peers`), below every trusted tier: …

The p2p network's peer set is one member (`MemberId::Peers`), below every trusted tier: `TrafficBalancer::new(trusted, Some(Peers { transport, in_flight }))`, `in_flight` ≥ `Peers::MIN_IN_FLIGHT` (3, one reserved per lane, no poll). `PeerTransport` = `headers(locator, stop)` + `block(hash)` (zebra picks the peer; the transport checks a body and blames its sender). It answers headers and blocks only (mempool bytes stay trusted: peers serve them by wtxid), is never benched (`report` on its ticket is a no-op) and never goes `Down`. A block no member serves logs a warning each minute it stays unserved.
