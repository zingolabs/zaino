# Snapshot pinning is unconditional

The driving port's snapshot carries its strongest guarantee: every read
through a snapshot observes the chain as of the pinned tip, and that data
stays readable while any clone of the snapshot lives — across reorgs,
without exception. We decided the guarantee is unconditional. An engine
that cannot retain the pinned view for as long as any clone lives is not
an implementation of the port.

We chose this because the alternative — letting a snapshot lapse — would
export the engine's storage policy into every driver's sync loop. A lapse
answer turns "retake and resync" into a code path every driver must write
and test for an event that a correctly built engine never produces, and it
demotes the port's strongest promise to a conditional one. The burden
lands instead on the adapter, which must retain the pinned view —
reference-count it, copy it, or hold the branch it was answered from —
while any clone lives. Hash-keyed reads over append-only finalised state
pin for free; the tip-relative surfaces (unspent outpoints at an address,
outpoint spend status) are what the adapter must keep answerable as of the
pinned tip after the chain moves on. The mock exemplifies the shape:
snapshots hold an `Arc` of the chain as of their creation, and a scripted
mutation swaps the `Arc` while live snapshots keep the old one.

One reorg race remains, and it lives outside the snapshot: taking a
snapshot can race the engine's view swap, and that failure crosses the
port as a transient backend failure. Reads through a snapshot never race
a reorg.

## Considered Options

We rejected pinning-with-lapse (a `Lapsed` answer any pinned read may
return once the engine has discarded the pinned view), because it forces
every driver to carry a lapse branch, weakens the guarantee that gives
snapshots their meaning, and shapes the port around one engine's retention
limits. We rejected leaving the guarantee unstated, because an
implementation would then satisfy the letter of the trait while serving
mixed-chain reads during reorgs — the exact torn-read hazard the snapshot
exists to exclude.
