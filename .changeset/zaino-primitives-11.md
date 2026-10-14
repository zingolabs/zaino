---
default: major
---

# `zaino-primitives`: byte wrappers expose their bytes as a public field

`Nullifier`, `NoteCommitment`, `EphemeralKey`, `TreeRoot`, `MerkleRoot`, `BlockCommitments` and `CompactCiphertext` drop their `From` conversions to and from byte arrays: build one as `Nullifier(bytes)` and read it as `nullifier.0`, as with `zcash_protocol::BlockHash`.
