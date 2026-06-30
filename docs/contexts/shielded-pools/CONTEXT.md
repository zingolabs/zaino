# Shielded Pools

Vocabulary for Zcash's shielded value pools and the per-pool components the
indexer reads from transactions and serves to wallets. Glossary only: no
implementation details, no design decisions.

## Language

**Shielded pool**:
A Zcash value pool whose notes are encrypted, identified by its own note
commitment tree and nullifier set. Distinct pools (Sapling, Orchard, Ironwood)
do not share trees.
_Avoid_: shielded protocol (reserve for the cryptographic construction), pool
type (reserve for the wire `PoolType` enum)

**Action**:
A single shielded operation that simultaneously spends one note and creates one
note within a pool. The Orchard and Ironwood pools express shielded value as
actions; Sapling uses separate spends and outputs.
_Avoid_: spend, output (those are the Sapling decomposition)

**Note commitment tree**:
The append-only Merkle tree of note commitments for one pool. Each pool has its
own; a wallet builds spend witnesses against the tree of the note's pool.
_Avoid_: commitment tree (ambiguous across pools), Merkle tree

**Ironwood**:
A shielded pool introduced at the NU7 network upgrade. It reuses the Orchard
protocol — its actions have fields identical to Orchard actions — but maintains
its own separate note commitment tree.
_Avoid_: NU7 (that names the upgrade, not the pool), "Orchard V3 pool"

**Ironwood action**:
An action belonging to the Ironwood pool. Compact-encoding-identical to an
Orchard action (the protocol reuses `CompactOrchardAction`), but it commits to
the Ironwood note commitment tree, not Orchard's.
_Avoid_: Orchard action (different pool and tree)

**NU7**:
The Zcash network upgrade that activates the Ironwood pool; zaino tracks its
activation height in `ActivationHeights` under the `nu7` key, aligned with
Zebra's `NetworkUpgrade::Nu7`.
_Avoid_: NU6.3 (an external roadmap's name for the same milestone), Ironwood
(the pool, not the upgrade)
