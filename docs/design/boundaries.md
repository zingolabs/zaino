# Boundaries

Where each kind of data lives, what Zaino believes and why, and where those lines are drawn in the
code. Every "should Zaino index this?", "can we take this from a peer?" and "should this be a
trait?" question is answered here once rather than re-argued.

## 1. Who owns which data

Three components each own one kind of data:

- **The validator (Zebra)** owns consensus: what is a valid transaction, a valid block, the
  admitted mempool and its fees. It should store and serve the minimum consensus requires, with no
  convenience indexes on top.
- **The wallet** owns key material, which never leaves it.
- **The indexer (Zaino)** owns everything else: every answer derived from the chain.

The corollary that does the work: data the validator does not need for consensus belongs in Zaino,
and data it does need stays in the validator rather than being copied.

## 2. What Zaino believes, and from whom

Zaino reads from two kinds of source ([chainview.md §1](./chainview.md#1-sources)):

- **Trusted validators**: zebrads the operator configured. Trust is configured, never discovered.
- **Peers**: any node on the Zcash p2p network. Discovered, unauthenticated, free to create.

The rule is that **nothing counts until it is verified, and only consensus decisions are taken on
trust**. What Zaino can check cheaply it checks itself, whoever supplied it; what would need a
second implementation of consensus it takes from a trusted validator, and only from one.

| Fact                              | Checked by Zaino                                                                                                       | Taken on trust, from a trusted validator only                                                                       |
| --------------------------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| block header, best tip            | proof of work, difficulty, time, linkage, most work ([chainview.md §2](./chainview.md#2-the-best-chain-proof-of-work)) | —                                                                                                                   |
| block body                        | hash linked to the verified header chain                                                                               | that it is valid (only a trusted validator holding it vouches for it: [chainview.md §4](./chainview.md#4-finality)) |
| transaction id                    | recomputed from the bytes                                                                                              | —                                                                                                                   |
| mempool membership, fee           | —                                                                                                                      | admission (each listing = it fully validated the transaction)                                                       |
| a mined transaction's bytes       | txid recomputed                                                                                                        | its location (`getrawtransaction 1`)                                                                                |
| a submission's fate               | spread, seen at a source that was never an entry                                                                       | the verdict: accepted or rejected                                                                                   |
| peers' claims (tips, inventories) | never counted, never voted                                                                                             | —                                                                                                                   |

Zaino does not validate transactions: that would be a second mempool verifier (proofs, nullifier
set, script, conflicts) kept in lockstep with every upgrade. A trusted validator answers the
question; Zaino asks it ([chainview.md §1](./chainview.md#1-sources)).

Peers never decide anything. A count of peers proves nothing an attacker cannot buy; proof of work
does. So peers are sources of headers and of whatever is checkable against them, of `inv`
sightings (`peers: x/y`, telemetry) and of submission entries, and never of claims.

## 3. What this decides for each RPC

An RPC is served from a Zaino index when its answer is derived: something Zaino computes that no
validator is obliged to have. Forwarding those would mean either a second implementation that can
disagree with the index, or a dependency on a validator feature such as `getaddressutxos` that
Zebra need not implement. So the block, tree-state and transparent-address methods all come from
an index, never falling back to a validator: while the indexes build they answer at the served
snapshot's tip (which trails the chain), and only before the first snapshot are they
`UNAVAILABLE`.

An RPC goes to the validators when it is a consensus decision or a primary object they already
hold for consensus reasons.

| Method                                                  | Served from                      | Why                                                                                                                                               |
| ------------------------------------------------------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GetBlock`, `GetBlockRange`, `GetLatestBlock`           | compact-block index              | derived: the compact projection                                                                                                                   |
| `CompactTx.fee` in a mined block                        | value-balance index              | derived: the spent prevouts' values                                                                                                               |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` | tree-state index                 | derived: the commitment-tree fold                                                                                                                 |
| `GetAddressUtxos`, `GetTaddressBalance` (+ streams)     | transparent-address index        | derived: the address rollup                                                                                                                       |
| `GetTaddressTransactions`                               | index + validators               | the index names the txids, a validator holds the bytes                                                                                            |
| `GetMempoolTx`, `GetMempoolStream`                      | chain view                       | admitted by a trusted validator; not in any block                                                                                                 |
| `CompactTx.fee` in `GetMempoolTx`                       | chain view                       | the admitting validator priced it (`getrawmempool true`)                                                                                          |
| `SendTransaction`                                       | chain view                       | a consensus decision: entries + a trusted verdict ([chainview.md §6](./chainview.md#6-submission-one-random-entry-node-watched-until-it-spreads)) |
| `GetTransaction`                                        | validators                       | a primary consensus object, looked up by txid (§4)                                                                                                |
| `GetLightdInfo`                                         | chain view + compact-block index | network state from a validator holding the verified tip; the height Zaino serves                                                                  |

The code enforces this in one place, `zaino-grpc`'s path dispatch (`service.rs`): each derived
method reads its index's reader through the request's snapshot only, and the validators are
reachable from exactly two routes,
`GetTransaction` and `GetTaddressTransactions`' bytes. A disabled index's methods answer
`UNIMPLEMENTED` naming the index, never a validator's answer instead; a test asserts that for
every index-backed path.

## 4. Why `GetTransaction` is the exception

`GetTransaction` looks like it wants an index, and does not.

Compact blocks are lossy by construction. `CompactTx` carries no serialized transaction, and each
shielded ciphertext is cut to 52 of its 580 bytes. So the bytes `GetTransaction` returns exist
nowhere in Zaino's stores, and serving it locally would mean a new store of full transaction bytes,
roughly an order of magnitude larger than the compact index and comparable to the chain itself.

The read pattern is also the worst case for that store. A wallet trial-decrypts the 52-byte head
from a compact block, and calls `GetTransaction` only when that succeeds, so only for its own
transactions. The reads are rare and reach into arbitrary history, so nothing could be pruned or
evicted: we would store all of it and read almost none of it.

The validator already holds those bytes because consensus requires it, and indexing them here
would be a second copy of data the boundary says lives there. `GetTaddressTransactions` follows the
same reasoning: the index supplies the derived `{height, txid}` set, and the bytes for each txid
come from a validator.

## 5. Why one fee has two sources

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

## 6. Seams in the code

A trait at a boundary earns its place only when there is, or must be able to be, more than one
implementation: another protocol version, another backend, or a deterministic fake that tests
cannot do without. Everything else is a concrete type.

| Seam                      | Trait                                                                                                                  | Implementations                                                                             | Why a trait                                                                              |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| a trusted validator's RPC | `zaino_source::ChainDataSource`                                                                                        | `ZebraRpcAdapter`; `MockChain` and per-test fakes                                           | future RPC versions; every layer above tested against data it controls                   |
| the p2p network           | `zaino_chainview::ValidatorP2pSource`                                                                                  | zainod's `ZebraPeers` over `zaino-peers`; `FakePeers`                                       | real peers need sockets and a multi-thread runtime; a second transport (Tor) is foreseen |
| storage                   | `zaino_persistence::PersistenceEngine` + `SequenceRead` / `MapRead` ([persistence-engine.md](./persistence-engine.md)) | `DiskEngine`: positional files (sequences), LSM (maps); LMDB and SQLite fit the same traits | one contract every index meets, whatever holds its bytes                                 |
| the filesystem            | `zaino_persistence::fs::Fs`                                                                                            | `RealFs`; `SimFs`                                                                           | crash-state enumeration needs a filesystem that can lose writes on demand                |

Deliberately not seams:

- **The gRPC service.** One endpoint, one protocol, one implementation: `GrpcService` over
  `Routes`, the enabled indexes as concrete readers in each `Snapshot`. A disabled index is
  `None`, not a stub.
- **The index readers and writers.** Each index is one concrete crate; what varies between them
  is data, not an interface to swap.
- **The validator set.** `zaino_traffic::TrafficBalancer` is the set of trusted validators (and
  peers): every request by class, tier, hedge and blame; it is not itself a `ChainDataSource`, so
  a pool is never mistaken for one validator.

## 7. Conversions at the edges

Bytes off disk or off the wire are where input is validated, so each conversion is a named function
whose signature states the direction and the failure, never `From`/`TryFrom` hiding both behind
`.into()` (`CLAUDE.md`):

- **Disk:** the index that owns a record owns its layout, as `encode(&X) -> [u8; N]` and
  `decode(&[u8; N]) -> X` (or a `Result` when some bytes are invalid), with a golden-bytes test
  beside it. The storage engine sees bytes only.
- **Wire:** domain crates (`zaino-primitives`, the indexes' domain types) never depend on
  `zaino-proto`. The crate that produces or consumes the wire bytes owns the conversion, for
  example `encode_compact_block` in compact-block's `build.rs`; a fallible wire → domain conversion
  returns an error enum naming each rejection.
- **Validator RPC:** `zaino-source` parses JSON-RPC into domain types and recomputes every hash it
  can from the bytes it was given (block hash from the header, txid from the transaction), never
  trusting a hash field in the JSON.
