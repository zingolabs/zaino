---
default: major
---

# `zaino-grpc`: `Routes.validators` is a `zaino_traffic::TrafficBalancer<S>`: `GetTransaction` and `GetTaddressTransactions` …

`Routes.validators` is a `zaino_traffic::TrafficBalancer<S>`: `GetTransaction` and `GetTaddressTransactions` bytes ask `transaction(txid)` (trusted by tier, absent → the next, hedged); `NOT_FOUND` only when every validator asked said absent, `UNAVAILABLE` on a failure or with none eligible.

**Migration:** Pass the same `TrafficBalancer` the chain view was built over (`balancer.clone()`).
