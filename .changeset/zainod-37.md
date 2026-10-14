---
default: minor
---

# `zainod`: `[[trusted_validators]] max_response_mib`, and a `fanout` bound checked at startup

`max_response_mib` (4 ..= 32, default 32) caps each validator's JSON-RPC replies; out of range fails the config parse. A `sync.lsm_store.fanout` whose segment lists could outgrow an enabled index's manifest is refused at startup instead of panicking on a later commit.
