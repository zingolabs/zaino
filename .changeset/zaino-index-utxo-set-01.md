---
default: major
---

# `zaino-index-utxo-set` (was `zaino-internal-value-balance`): the transparent UTXO set index

The crate, its types (`UtxoSetReader`, `UtxoSetIndexWriter`), `IndexKind::UtxoSet` and the index name (`utxo_set`: metric label, `/status`, `zainod verify`, on-disk directory, snapshot archive entry) are renamed: it stores the transparent UTXO set, and "value balance" is the protocol's shielded-pool term.
