# Decouple the mempool from chain state in Zaino's driving port

Zallet today learns of a new block through a side effect: Zaino's mempool
stream ends when the chain tip changes, and the sync loop treats that closure
as its only new-block signal (zcash/zallet,
`zallet-core/src/components/sync.rs:136`). We decided that the driving port
Zallet will consume from Zaino must not carry this coupling. The port serves
the mempool through an abstraction that stands apart from chain state, because
the protocol itself imposes no structural relation between them: a transaction
commits to no block hash. Consensus binds a transaction only to chain
*state* — shielded anchors, nullifier non-membership, transparent prevouts,
expiry height, and the consensus branch id — so a mempool view is coherent
only relative to the tip it was validated against, and the port expresses
exactly that.

The port therefore provides an explicit chain tip-change subscription, and its
mempool capability is an independent stream whose view is tagged with the tip
it was validated against. Zallet composes the two: on a tip event it resyncs
and resubscribes to the mempool. This costs Zallet a sync-loop rewrite during
adoption.

## Considered Options

We rejected canonizing the closure idiom (keeping "stream ends on tip change"
as the contract), because it hides a chain-state dependency in the mempool
abstraction's fine print and forces every future engine to implement the
entanglement. We rejected tip polling by Zallet, because it adds latency and
busy-work to every implementation's hot path.
