---
default: major
---

# `zaino-primitives`: transparent scripts and outpoints are librustzcash's; `BlockHeader` → `DecodedHeader`

- `TransparentOutput.script` is `zcash_transparent::address::Script` and `TransparentData.inputs` holds `zcash_transparent::bundle::OutPoint`. zaino-primitives' own `Script`, `OutPoint` (with `encode`/`decode`/`LEN`) and `testing::outpoint` are removed.
- `BlockHeader` (consensus fields + its hash and height) is renamed `DecodedHeader`, so it no longer shadows librustzcash's raw `BlockHeader`.
- `zaino-index-transparent-address`: `AddressUtxo` is no longer `Copy` (its `outpoint` is librustzcash's).

**Migration:** `Script::new(bytes)` → `Script(Code(bytes))` (`zcash_script::script::Code`), `as_bytes()` → `.0.0`; `OutPoint { txid, vout }` / `outpoint(txid, vout)` → `OutPoint::new(txid_bytes, vout)`, `.txid` → `.txid()` / `.hash()`, `.vout` → `.n()`; `BlockHeader` → `DecodedHeader`. On-disk layouts are unchanged.
