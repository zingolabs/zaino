---
default: minor
---

# `zainod`: With `[p2p]` on, zainod keeps serving through an outage of every trusted validator

With `[p2p]` on, zainod keeps serving through an outage of every trusted validator. Peers' headers extend the tip when they pass every header rule against zainod's own context (proof of work, difficulty, time, linkage), and any block the verified chain names is fetched by hash from a peer, its body checked against its header (a mismatch blames the peer to zebra). Finality moves on peers' word once outbound peers in more than 5 netgroups carry the chain and its difficulty has not collapsed since the last vouch; otherwise it waits for a trusted validator. While none vouches and the best block is fresh (its time within 30 min of the clock), `/readyz` answers `200` with `state` `degraded`, and `GetLightdInfo` answers from the served tip and the upgrade schedule. A stale best is `down`. `p2p.peer_target` must be at least 3.
