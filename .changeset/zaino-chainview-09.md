---
default: minor
---

# `zaino-chainview`: `HeaderSync::with_peers(params)`: every 5 s, headers past the chain's best are asked of the p2p peer set …

`HeaderSync::with_peers(params)`: every 5 s, headers past the chain's best are asked of the p2p peer set (`HeaderAsk::Peers` by locator) and inserted untrusted (`insert_untrusted`: every header rule against the chain's own context), capped at the ceiling. Peer headers extend the tip past a down validator but are never vouched (finality on peers' word is its own rule, above). A header failing a rule is `report`ed. Anchoring keeps the difficulty context below the final tip (`anchor_below()`).
