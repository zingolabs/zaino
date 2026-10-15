---
default: patch
---

# `zainod`: Boot, hold and validator-config fixes

- A regtest validator whose upgrade schedule lists no Sapling is refused at boot with a clear error instead of aborting on the first `GetTreeState`.
- More than 64 `[[trusted_validators]]` is a config error instead of an assertion failure.
- SIGTERM / SIGINT is handled from the start of boot: a signal during the index-snapshot download (aria2c is stopped with it) or while regtest waits on its first validator exits 0.
- `zainod hold`: while held, every gRPC connection, open or accepted later, gets a GOAWAY `hold_goaway_delay_secs` after the hold was seen (a hold before the gRPC listener binds included; a hold, resume, hold flap counts from the last hold). `/readyz` reports a held node's drain in `detail` (`GOAWAY pending`, then `GOAWAY sent`), and `zainod hold` waits for `GOAWAY sent` (or, without an admin listener, for the longest a running zainod can take) before it prints `safe to stop`; it no longer claims that for an old marker.
- `validator_client = "grpc"` without an `indexer_address` fails boot before anything binds, from the client itself (one check). Config keys, defaults and `generate-config` output are unchanged.
