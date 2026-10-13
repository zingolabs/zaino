---
default: minor
---

# `zainod`: `[[trusted_validators]]`: every entry equal, at least one

`[[trusted_validators]]`: every entry equal, at least one. Each is a link budget zainod never exceeds (`max_connections`, default 32, at least 5: one for polling, the rest split into control, interactive and bulk lanes; optional `max_mib_per_sec`), with `connect_timeout_secs` (2) and `read_timeout_secs` (30, silence rather than total duration). A validator down at boot or later is retried every 30 s and never stops zainod. Optional `indexer_address` names zebrad's indexer gRPC, whose tip and mempool push streams wake polling at once.
