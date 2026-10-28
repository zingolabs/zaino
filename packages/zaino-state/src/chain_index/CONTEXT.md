# Chain Index — glossary

Canonical terms for the chain-index domain. Glossary only: no implementation
details, no design decisions. When a term below conflicts with usage in code
or discussion, this file wins until deliberately revised.

## Non-finalized state (NFS)
The in-memory cache of blocks within the reorg-possible window near the chain
tip. Holds full blocks, may temporarily hold competing branches.

## Finalized state
The on-disk store of blocks deep enough below the tip to be considered
irreversible. Append-only: never incrementally rolled back.

## Non-finalized depth
The size of the reorg-possible window, in blocks below the best-known tip.
Tracks the validator's (zebra's) maximum reorg height. Blocks within this
depth of the tip live in the NFS; blocks below it are finalized.

## Finalized floor
The height below which blocks are finalized and therefore evicted from the
NFS. Derived from the best-known tip minus the non-finalized depth, clamped
at genesis. After a chain-shortening reorg the floor can move backward while
the on-disk finalized height does not.

## Seam
The single height at which the NFS's lowest retained block meets the
finalized state's tip. The two layers must reference the same block at this
height — the seam is where they overlap, not a gap or an overlap of many
blocks.

## Eviction
Removal of a block from the NFS once the finalized floor rises past its
height. A block is evicted when it passes below the seam.
