# Zaino glossary

One word per concept; names, docs and logs use these. System design: [docs/architecture.md](docs/architecture.md).

## Zaino terms

| Term | Meaning | Not / avoid |
| --- | --- | --- |
| **Finalized** | Blockchain: at or below the finalized tip, never reorged. `FinalizedStream`, `finalized_tip` | "final", "finalised" (outside the frozen `finalised_depth` key); never for storage state |
| **Non-finalized** | Blockchain: above the finalized tip, still reorg-able | "unfinal"; never for "not yet on disk" |
| **Finality depth** | Blocks between best and finalized tip (`FinalityDepth`, `[sync] finalised_depth`) | reorg depth |
| **NFS** (non-finalized state, `zaino-nfs`) | Every block above the indexes' **durable** tips, folded in RAM and served: non-finalized blocks *and* finalized ones not yet durable. Its root = the lowest durable tip | "everything non-finalized" (it also holds finalized, not-yet-durable blocks) |
| **Durable** | Persistence: on disk, committed by the store; survives a crash (`Store::durable()`, `DurableTip`) | "committed" as a state, "final" |
| **Applied** | Persistence: handed to a store (`apply`), in its buffer, not yet durable (`AppliedView`, `AppliedLayer`) | "uncommitted", "staged" |
| **commit** | The action that makes applied changes durable | "committed" / "uncommitted" as a state |
| **Pinned view** | Owned read state, fixed while held (`PinnedView`): a durable view, or NFS layers over one | "snapshot read" |
| **Fold** | One block → one index's changes (`fold(parent, block, out)`) | the chain view's poll **merge** (`ObservationMerge`) |
| **Best tip** | Most-work tip of the header chain | "chain tip" alone |
| **Served tip** | What gRPC answers at: deepest folded best-chain block, else the NFS root | best tip (they differ while folding) |
| **Finalized tip** | min(highest vouched, best − depth), or best − depth on peers' word | final tip |
| **GrpcSnapshot** | What gRPC serves: one atomic publication of chain view + NFS, one load per request (`zaino-grpc-snapshot`) | bare "snapshot" |
| **IndexSnapshot** | Index archive downloaded to fill empty indexes (`[snapshot]` config, `IndexSnapshotConfig`) | bare "snapshot" |
| **HeaderChainSnapshot** | The header chain's published state: every branch above the finalized tip, the best tip, each block's witnesses (`zaino-header-chain`) | "verified chain" (trusted headers enter unchecked; no block body is checked) |
| **ChainViewSnapshot** | The chain view's published state: header chain, mempool, per-validator facts | bare "snapshot", "view" alone |
| **Trusted validator** | A validator the operator configured (`[[trusted_validators]]`, `TrustedValidator*`) | endpoint, node, member |
| **Peer** | Any node on the Zcash p2p network, unauthenticated (`Peer*`) | validator |
| **Member** | `zaino-traffic` balancer only: one trusted validator, or the whole peer set (`MemberId`) | for a trusted validator anywhere else |
| **Indexer** | Zaino as a whole (`IndexerError`, `zainod`) | one index |
| **Index** | One served data index (`Index*`, `IndexKind`) | indexer |
| **ZebraIndexer** | zebrad's indexer gRPC: blocks + push streams (`ZebraIndexer*`, `indexer_address`) | "indexer" alone |
| **`zaino-index-*` / `zaino-internal-*`** | Crates of indexes gRPC serves / indexes only Zaino reads (block hash → height) | — |
| **Locator / quantity** | Locator = height, tx / output / subtree index: big-endian. Quantity = amount, size, count, time: little-endian ([rule](CLAUDE.md#disk-and-wire-boundaries-named-functions-not-fromtryfrom)) | — |

## Chains and pools

| Term | Meaning | Not / avoid |
| --- | --- | --- |
| **The Public Testnet** | The public Zcash test network only. Non-hermetic: state shared, and a left epoch (pre-NU6.3, since 4,134,000) cannot be re-entered | bare "testnet" for a locally launched chain |
| **Regtest net** | Any hermetic, locally launched chain whose activation heights the launcher picks, whatever network-kind flag it runs under | local testnet, custom testnet |
| **Ironwood / Orchard** | Eras, fixtures and predicates are named by pool, paired with the other pool (`ORCHARD_ONLY_*` ↔ `IRONWOOD_ONLY_*`). **NU6.3** names only the upgrade: heights, branch ID, rules | mixing pool and upgrade names (`NU6_3_ACTIVE_*`) |
| **Unfiltered pool set** | Pools served for an empty `poolTypes`: Sapling, Orchard, Ironwood, transparent excluded (`zaino_index_compact_block::Pools::default`). Narrower = compact blocks disagree with their `chainMetadata` sizes, a phantom reorg to a scanning wallet | default pools, backfill set, "Sapling and Orchard" |
| **Cross-address restriction** | Post-NU6.3 Orchard Action rule: an output carries the spent note's `(g_d, pk_d)`, so each action is change to its own address or a withdrawal ([spec](https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction)) | "exit-only" (same-receiver change stays in the pool) |
