---
default: minor
---

# `zaino-primitives`: Feature `testing`: `testing::MockChain`, one regtest block-tree builder for every test (`regtest()`, …

Feature `testing`: `testing::MockChain`, one regtest block-tree builder for every test (`regtest()`, `upgrades(Upgrades)`, `varied_work()`, `network(..)`, `genesis_with(..)`, `mine(|b| ..)`, `mine_empty(n)`, `branch(parent)` / `fork(height)` → `Branch` with `outweigh()`, `tip()`, `at(height)`, `blocks(tip)`, `block(hash)`, `header_bytes(hash)`, `fees(hash)`, `tx_bytes(txid)`, `blockchain_info(tip)`, `schedule()`), with `BlockBuilder` (`coinbase`, `tx`, `raw_tx`, `time`, `bits`), `TxBuilder` (`txid`, `spend`, `pay`, `sapling_spend`, `sapling_output`, `orchard_action`, `ironwood_action`, `value_balance`, `sprout_balance`, `fee`), `Upgrades` (`all_at`, `with`, `onward`, `without`, `activation`), `Work`, `Schedule`, and the helpers `h`, `p2pkh`, `outpoint`, `fee_left`. Every block it hands out is checked: real linked headers, merkle root over its txids, one coinbase, spends of unspent outputs on its own branch, conserved value and stated fees, pool data only once its upgrade is active; `outweigh()` reorgs by the least work that wins.
