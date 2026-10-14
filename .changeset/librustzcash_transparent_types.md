---
default: major
---

# `zaino-primitives`: txids, transparent scripts and outpoints are librustzcash's; `BlockHeader` → `DecodedHeader`

- Every txid is `zcash_protocol::TxId` (`Transaction.txid`, `PeerTxId.txid`, every source, balancer, chain-view and index API). zaino-primitives' own `TransactionId` is removed.
- `TransparentOutput.script` is `zcash_transparent::address::Script` and `TransparentData.inputs` holds `zcash_transparent::bundle::OutPoint`. zaino-primitives' own `Script`, `OutPoint` (with `encode`/`decode`/`LEN`) and `testing::outpoint` are removed.
- `BlockHeader` (consensus fields + its hash and height) is renamed `DecodedHeader`, so it no longer shadows librustzcash's raw `BlockHeader`.
- `zaino-index-transparent-address`: `AddressUtxo` is no longer `Copy` (its `outpoint` is librustzcash's).

**Migration:** `TransactionId::from(bytes)` → `TxId::from_bytes(bytes)`, `str::parse::<TransactionId>()` → `TxId::from_hex(s)` (an `Option`); `Script::new(bytes)` → `Script(Code(bytes))` (`zcash_script::script::Code`), `as_bytes()` → `.0.0`; `OutPoint { txid, vout }` / `outpoint(txid, vout)` → `OutPoint::new(txid.into(), vout)`, `.txid` → `.txid()`, `.vout` → `.n()`; `BlockHeader` → `DecodedHeader`. `Debug` of a txid prints the full display-order hex. On-disk layouts are unchanged.
