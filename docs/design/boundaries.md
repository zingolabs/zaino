# Where data lives

Three components, one rule each. It decides every "should Zaino index this?"
question, so it is written down rather than re-argued.

- **Validator (Zebra)** — consensus-critical data only. It should store and
  serve the minimum consensus requires, and no convenience indexes on top.
- **Wallet** — key material. Never leaves it.
- **Indexer (Zaino)** — everything else.

The corollary that does the work: **data the validator does not need for
consensus belongs in Zaino**, and **data it does need stays there** rather than
being copied.

## What this decides

An RPC is served from a Zaino index when its answer is *derived* — something
Zaino computes that no validator is obliged to have. Forwarding those would mean
either a second implementation that can disagree with the index, or a dependency
on a validator feature (`getaddressutxos`, `getaddressbalance`) Zebra need not
implement. So `GetAddressUtxos`, `GetTaddressBalance`, `GetTreeState`,
`GetSubtreeRoots`, `GetBlock*` all come from the index, and **fail while the
index is still building rather than falling back to a validator**.

An RPC is forwarded when it is a point lookup of a primary object the validator
already holds for consensus reasons, or a write.

| Method                                                  | Where             | Why                                              |
| ------------------------------------------------------- | ----------------- | ------------------------------------------------ |
| `GetBlock`, `GetBlockRange`, `GetLatestBlock`           | index             | derived (compact projection)                     |
| `CompactTx.fee` in a mined block                        | index             | derived (value-balance: Σ spent prevouts)        |
| `CompactTx.fee` in `GetMempoolTx` / `GetMempoolStream`  | validator         | its admission computed it (`getrawmempool true`) |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` | index             | derived (commitment-tree fold)                   |
| `GetAddressUtxos`, `GetTaddressBalance`                 | index             | derived (address rollup)                         |
| `SendTransaction`                                       | validator         | a write                                          |
| `GetTransaction`                                        | validator         | primary consensus object, point lookup           |
| `GetLightdInfo`                                         | validator + index | node liveness + our served height                |

`ValidatorPorts` in `zaino-grpc/src/validator.rs` is the enforcement: it names
exactly the ports the forwarded methods need. An index-backed method physically
cannot forward, because there is no port to forward to.

## Why `GetTransaction` is the exception

It is the case that looks like it wants an index and does not.

Compact blocks are lossy by construction — `CompactTx` carries no serialized
transaction, and each shielded ciphertext is truncated to 52 of 580 bytes. So
the bytes `GetTransaction` returns exist nowhere in Zaino's stores; serving it
locally means a **new** store of full transaction bytes, roughly an order of
magnitude larger than the compact index and comparable to the chain itself.

And the read pattern is the worst possible case for that store. A wallet
trial-decrypts the 52-byte head from a compact block and calls `GetTransaction`
only when it succeeds — so only for its own transactions. Rare reads, of
arbitrary history, so nothing can be pruned: store all of it, read almost none
of it, never evict.

Meanwhile the validator already holds those bytes because consensus requires it.
Indexing them here would be a second copy of data the boundary says lives there.

`GetTaddressTransactions` rides on the same reasoning: the index supplies the
`{height, txid}` set (derived), and the bytes are hydrated per txid from the
validator (primary).

## Why one fee has two sources

A mined transaction's fee is derived: the value-balance index holds every
transparent output's value, so it resolves the one term a block does not carry
(what each input spends) itself. A mempool transaction is not part of any fold.
It may spend another unconfirmed transaction's output, which no index holds,
and the validator already priced it to admit it. So the mempool fee is read off
the validator's own listing rather than re-derived, the same way
`GetTransaction` bytes are. `CompactTx.fee` is only a `uint32`, so a fee that
is unknown (a coinbase, our own broadcast not yet listed) or ≥ 2^32 zatoshis is
sent as 0, which the protocol defines as "not provided".
