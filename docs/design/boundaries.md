# Where data lives

Three components each own one kind of data, and that rule decides every "should Zaino index this?"
question, so we write it down once rather than re-argue it.

- **The validator (Zebra)** owns consensus-critical data. It should store and serve the minimum
  consensus requires, with no convenience indexes on top.
- **The wallet** owns key material, which never leaves it.
- **The indexer (Zaino)** owns everything else.

The corollary that does the work is that data the validator does not need for consensus belongs in
Zaino, and data it does need stays in the validator rather than being copied.

## What this decides

An RPC is served from a Zaino index when its answer is derived: something Zaino computes that no
validator is obliged to have. Forwarding those would mean either a second implementation that can
disagree with the index, or a dependency on a validator feature such as `getaddressutxos` or
`getaddressbalance` that Zebra need not implement. So the block, tree-state and transparent-address
methods all come from the index, and while an index is still building they fail with UNAVAILABLE
rather than falling back to a validator.

An RPC is forwarded when it is a write, or a point lookup of a primary object the validator already
holds for consensus reasons.

| Method                                                  | Where             | Why                                                |
| ------------------------------------------------------- | ----------------- | -------------------------------------------------- |
| `GetBlock`, `GetBlockRange`, `GetLatestBlock`           | index             | derived: the compact projection                    |
| `CompactTx.fee` in a mined block                        | index             | derived: value-balance sums the spent prevouts     |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` | index             | derived: the commitment-tree fold                  |
| `GetAddressUtxos`, `GetTaddressBalance`                 | index             | derived: the address rollup                        |
| `GetTaddressTransactions`                               | index + validator | the index names the txids, the validator the bytes |
| `GetMempoolTx`, `GetMempoolStream`                      | validator         | not in any block                                   |
| `CompactTx.fee` in `GetMempoolTx`                       | validator         | its admission computed it (`getrawmempool true`)   |
| `SendTransaction`                                       | validator         | a write, relayed to every validator in the view    |
| `GetTransaction`                                        | validator         | a primary consensus object, looked up by txid      |
| `GetLightdInfo`                                         | validator + index | node and network state, plus our served height     |

The code enforces the split. `ValidatorPorts` in `zaino-grpc/src/validator.rs` gives the
single-validator handler only `SendRawTransaction` and `GetTransaction` (`GetLightdInfo` reads the
chain view's copy of each validator's `getblockchaininfo`), so an
index-backed method has no validator port it could forward to. If an operator disables an index,
its methods answer UNIMPLEMENTED instead.

## Why `GetTransaction` is the exception

`GetTransaction` looks like it wants an index, and does not.

Compact blocks are lossy by construction. `CompactTx` carries no serialized transaction, and each
shielded ciphertext is cut to 52 of its 580 bytes. So the bytes `GetTransaction` returns exist
nowhere in Zaino's stores, and serving it locally would mean a new store of full transaction bytes,
roughly an order of magnitude larger than the compact index and comparable to the chain itself.

The read pattern is also the worst case for that store. A wallet trial-decrypts the 52-byte head
from a compact block, and calls `GetTransaction` only when that succeeds, so only for its own
transactions. The reads are rare and reach into arbitrary history, so nothing could be pruned or
evicted: we would store all of it and read almost none of it.

Meanwhile the validator already holds those bytes because consensus requires it, and indexing them
here would be a second copy of data the boundary says lives there. `GetTaddressTransactions` follows
the same reasoning. The index supplies the derived `{height, txid}` set, and the bytes for each txid
come from the validator.

## Why one fee has two sources

A mined transaction's fee is derived. The value-balance index holds every transparent output's
value, so it resolves the one term a block does not carry, what each input spends, by itself.

A mempool transaction is not part of any fold. It may spend another unconfirmed transaction's
output, which no index holds, and the validator already priced it in order to admit it. So we read
the mempool fee off the validator's own listing rather than re-deriving it, the same way we fetch
`GetTransaction` bytes. Zebra lists the actual fee (`miner_fee`), not the ZIP 317 conventional one.
`GetMempoolStream` sends `RawTransaction`s, so it carries no fee at all.

`CompactTx.fee` is a `uint32` with no presence bit ("present if server can provide"). As a Zaino
policy, a coinbase, which pays no fee, a mempool transaction not yet priced, and a fee of 2^32
zatoshis or more are all sent as 0 rather than as a saturated wrong value.
