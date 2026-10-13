---
default: minor
---

# `zaino-index-transparent-address`: First release

First release. The transparent-address index: two insert-only maps, `receives` (scoped by address) and `spent` (by outpoint) (`schema(network)`), each a pure projection of a block (`fold`). `TransparentAddressReader::as_of(tip).with_max_rows(n)` answers `utxos`, `utxos_of`, `balance`, `balances` and `transactions`, refusing a walk past `n` receives (`DEFAULT_MAX_ADDRESS_ROWS` = 100,000) rather than answering short. `TransparentAddressIndexWriter::new(store)` follows the final stream.
