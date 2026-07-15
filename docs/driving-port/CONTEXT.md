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
