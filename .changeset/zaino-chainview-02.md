---
default: major
---

# `zaino-chainview`: Every request goes through a `zaino_traffic::TrafficBalancer`: `ChainView::new(addresses, balancer, depth)` …

Every request goes through a `zaino_traffic::TrafficBalancer`: `ChainView::new(addresses, balancer, depth)` (one address per trusted member, for logs and status); `observation_fold()` returns the `ObservationFold` to spawn, folding each member's poll (`observe`) and fetching new mempool bytes with `bytes(..)`; each poll asks `getblockhash` at the view's best (`poll_best`); `header_sync(chain)` asks `headers(Pinned)` and `report`s a validator serving an undecodable or rule-breaking header (benched); a submission's trusted entries are the balancer's `entries()`, each push `submit(member, raw)`. `ValidatorMetadata.health` (`zaino_traffic::Health`) replaces `state`; latency and failures move to the balancer's `members()`. Removed: `EndpointPoller`, `PollWaker`, `Endpoint`, `EndpointState`, `Ewma`.

**Migration:** Build the balancer first, then `ChainView::new(addresses, balancer.clone(), depth)`; spawn `view.observation_fold().run(cancel)` where the pollers ran and wire push streams to `balancer.pushed(member, Push::Changed | Push::Link(up))`. Read health from `ValidatorMetadata.health`, latency and failures from `balancer.members()`.
