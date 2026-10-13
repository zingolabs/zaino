---
default: major
---

# `zaino-primitives`: Types only the removed JSON-RPC and finalised-state stack used are removed: the `types::rpc` module, …

Types only the removed JSON-RPC and finalised-state stack used are removed: the `types::rpc` module, `AddressBalance`, `AddressDelta`, `BlockTxPosition`, `BlockVerbose`, `BlockTreeSizes`, `ValuePoolBalance`, `ChainStateEpoch`, `CompactBlock`, `PreIndexCompactBlock`, `PreIndexCompactTx`, `CompactCiphertextLength`, `BlockConfirmations`, `TxConfirmations`, `ConfirmationsCodecError`, `IndexId`, `MempoolInfo`, `Outpoint`, `TransparentAddress`, `TransparentAddressError`, `TransparentAddressKey`, `ScriptType`, `classify_script`, `TreeRootInfo`, `TreeRoots`, `TxOutSetInfo`, `Utxo`, `ZatoshisFlowSum`, `BlockError` and the `Difficulty`, `SubtreeIndex` and `TxIndex` aliases. The chain-work types (`AbsoluteChainWork`, `SingleBlockWork`, `RelativeChainWork`, `WorkOverflow`, `WorkUnderflow`, `ChainWorkBytesError`) and `CompactDifficulty::{to_work, as_bits, try_from_be_bytes}` are removed too. `Height::{abs_diff, depth_from}` and `BlockRef::from_tip` are gone. The crate no longer depends on `zcash_address`.

**Migration:** Use `zcash_transparent`'s address types for transparent addresses, `transaction::OutPoint` for `Outpoint`, and `CompactDifficulty::try_from_bits` to validate nBits.
