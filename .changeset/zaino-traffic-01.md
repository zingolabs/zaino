---
default: minor
---

# `zaino-traffic`: First release

First release. One scheduler for every request to trusted validators and peers (`TrafficBalancer::new(trusted, peers)` over any `zaino_source::ChainDataSource`, peers through the `PeerTransport` port; `TrafficDriver::run(cancel)` its one task). Asks by class (`block(hash, Tip | Bulk)`, `headers(Pinned | Peers)`, `bytes`, `transaction`, `submit`), each `Answered { value, from, ticket }`; trusted members by `priority` tier, then peers, the least loaded within a tier; per member one permit for its poll (never delayed by an ask) and three lanes (control: submissions, headers; interactive: tip blocks, lookups, mempool bytes; bulk: bulk blocks), each with one reserved permit, control dispatched first (`Limits::MIN_CONNECTIONS` = 5); hedges at a fixed class floor (tip block 2 s, lookup 1 s, bulk block 15 s); a failure moves to the next member, each asked once per round; `report(ticket, why)` benches exactly the sender (60 s doubling to 1 h, `zaino_traffic_misanswers_total`); dropping an ask abandons it. Every trusted member polled (1 s, 15 s streaming, 200 ms floor, 0.5 → 30 s ladder; `Down` after 10 failures), `getblockhash` at the `poll_best` height read as each poll starts, `pushed` wake-ups; `observe(member)` = its latest `Observation`, `members()` = the `MemberTable`, `entries()` = submission entries.
