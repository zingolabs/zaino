---
default: minor
---

# `zainod`: One traffic balancer sends every request to the trusted validators (polls, headers, blocks, lookups, …

One traffic balancer sends every request to the trusted validators (polls, headers, blocks, lookups, submissions): per-class reserved connections, the cheaper of two by latency × load, hedges, one retry budget, and a validator serving a block or header that fails its check benched for 60 s (doubling; WARN `Misanswer, member benched`, `zaino_traffic_misanswers_total`). New `[[trusted_validators]] priority` (default 0): lower tiers first, a higher tier taking overflow and hedges. `/statusz` validator latency and failures are the balancer's. The upgrade schedule is read from the first answered poll.
