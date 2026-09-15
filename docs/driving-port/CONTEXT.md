# The Zallet driving port

This context covers the contract through which Zallet (and later other
consumers) drives Zaino. The language below was settled in a design review
held in the Zallet repository, where the consumer side of this contract lives.

## Language

**Mempool**:
The set of transactions awaiting mining. It is chain-validity-dependent but
chain-structure-independent: consensus binds its transactions to chain state
(anchors, nullifiers, prevouts, expiry, branch id), yet its representation and
service abstraction stand apart from chain state. A mempool view is coherent
only relative to the chain tip it was validated against.

**Driving port**:
The contract through which consumers drive Zaino. It spans chain reads,
mempool streaming, and transaction broadcast. Zallet is its first driver, and
zainod-for-lightclients is its expected second; zingolib is not a consumer.
_Avoid_: primary port, wallet API, indexer interface

**Snapshot**:
A pinned view of the best chain, taken through the port. Every read through
a snapshot observes the chain as of the tip it was pinned to, and that data
stays readable while any clone of the snapshot lives — across reorgs. The
guarantee is unconditional: an engine must retain the pinned view for as
long as any clone lives, and an engine that cannot is not an implementation
of the port. The pinned tip is a property of the snapshot, not a query
against the engine.
_Avoid_: view (Zallet's consumer-side name for the same idea)

**Conformance kit**:
The executable half of the port contract: a suite of invariant cases that
every implementation of the port must pass, shipped with the port itself.
An engine adapter passing the kit is what qualifies it as an implementation.

**Block locator**:
A descending-by-height sample of blocks a driver believes are on the chain,
offered to the port to locate the fork point between the driver's view and
the best chain. A block's identity is its hash; its height is position, so
presence is always judged by hash.

**Reported upgrades**:
The network-upgrade schedule as the validator reports it, ascending by
activation height, each upgrade active or pending relative to the current
tip. The port passes the validator's schedule through — activation heights
come from the validator, never from constants of the port's own — and
drivers feed it into their node-compatibility checks.

**Broadcast**:
Submission of a transaction to the network through the port — there is no
side channel. Acceptance returns the txid and means the engine admitted the
transaction to its mempool, where the mempool stream makes it observable.
Rejection is a domain answer (malformed bytes, or a validation rejection
with the engine's reason), distinct from a backend failure.

**Tip event**:
The port's explicit signal that the best chain moved, carrying the new tip.
A fresh subscription delivers the current tip first; events may coalesce
under load, but the latest tip always arrives; a reorg is a tip change like
any other, and the new tip may sit at or below the old height. This is how
drivers learn of new blocks — never through a side effect of another stream.

**Fork point**:
The block a driver's view and the pinned best chain last share: the locator
entry whose block sits highest on the pinned chain, judged by hash and never
by the heights the locator claims. Everything the driver holds above the
fork point is not on the pinned chain. Views sharing no block have no fork
point.

**Treestate**:
The note commitment tree state of every shielded pool — Sapling, Orchard,
and Ironwood — as of one block of the pinned chain. Every in-view height has
a treestate; what varies per pool is whether its frontier is present, and an
absent frontier means an empty tree, never an error (zcash/zallet#455).

**Transaction status**:
Where a snapshot places a transaction: mined in the pinned best chain,
orphaned onto a non-best branch, or unknown. The status speaks only of chain
state — mempool presence is deliberately not a status, because the mempool
stands apart from chain state and is observed through its own stream.

**Spend status**:
Whether the pinned view considers a transparent output spent. Spentness is
authoritative — it comes from the engine's UTXO set — while naming the
spending transaction may need a per-outpoint spend index the engine does not
maintain; an engine that knows the output is spent but not by whom says so
explicitly, and the driver retries rather than concluding the output is
unspent (ZcashFoundation/zebra#10806). An outpoint no in-view transaction
created has no spend status at all.

**Mempool view**:
The driver-side accumulation of one mempool subscription: the engine's
mempool as of the tagged tip, plus possibly transactions the engine has
since evicted. The stream signals arrivals only, so the view is a
superset of the engine's mempool, trued up by resubscribing; mempool
presence is a hint, never authoritative.

**Transient failure**:
A backend failure likely to resolve on retry, such as taking a snapshot
while the engine's view is mid-swap. Reads through a snapshot never race a
reorg — pinning is unconditional — so the reorg window touches only the
unpinned surface. Every backend failure crossing the port is classified as
transient or fatal, and drivers decide retry from that classification
alone. A domain rejection is an answer, not a failure, and is never
transient.
