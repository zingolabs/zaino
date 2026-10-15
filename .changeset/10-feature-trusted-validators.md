---
default: minor
---

# `zainod`: several trusted validators, one traffic balancer

`[[trusted_validators]]` takes 1 to 64 entries, all equal: no vote, and more never weakens the
view. The served tip is the most-work header chain any of them serves, served while one holds it.
Two or more survive one going down.

Per entry:

| Key | Default | Meaning |
| --- | --- | --- |
| `jsonrpc_address` | `127.0.0.1:8232` | Zebra JSON-RPC |
| `cookie_path` / `user` + `password` | none | authentication |
| `connect_timeout_secs` | 2 | |
| `read_timeout_secs` | 30 | silence before a request fails, not total duration |
| `max_connections` | 32 | link budget, at least 5 (one for the poll, one reserved per lane + one shared) |
| `max_mib_per_sec` | unlimited | response bytes paced to this rate |
| `max_response_mib` | 32 | largest JSON-RPC reply, 4 ..= 32; batches split to fit |
| `priority` | 0 | lower tiers asked first; a higher tier takes overflow and hedges only |
| `indexer_address` | none | zebrad's indexer gRPC: tip and mempool push streams wake polling at once |
| `[trusted_validators.indexer_tls]` | plaintext | mutual TLS to `indexer_address`: `ca_path`, `cert_path`, `key_path`, optional `server_name` |

- One balancer schedules every request (polls, headers, blocks, lookups, submissions) in three
  lanes per validator (control, interactive, bulk), so a bulk sync never starves the tip.
- A validator serving a block or header that fails its check is benched for 60 s, doubling up to
  an hour (`Misanswer, member benched`, `zaino_traffic_misanswers_total`).
- A validator down at boot or later is retried with backoff and never stops zainod. On mainnet
  and testnet zainod boots without one answering (the upgrade schedule is compiled in); regtest
  reads it from the first validator that answers.
- `validator_client = "grpc"` (default `legacy_rpc`) fetches blocks over each validator's
  `indexer_address` as raw bytes instead of hex inside JSON, everything else over JSON-RPC. Every
  entry then needs `indexer_address`, and a zebrad serving the indexer's `GetBlockRange`
  (Zaino's zebra fork).
