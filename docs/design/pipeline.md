# The indexing pipeline

Status: **implemented** (2026-10-08). `zaino-header-chain` is still a separate crate (merging into
`zaino-chainview` next).

Zaino turns a validator's chain into five indexes and serves them. Every block takes one of two
paths, and the two paths never meet:

- **Final path (durable).** A final block goes to every index writer, which folds and commits
  it. The writers never see a reorg.
- **Tip path (RAM).** A block above the final tip is folded in memory by the NFS, served at once,
  and dropped once it is durable.

**Trusted validators are trusted.** A trusted validator validated every block it holds, so Zaino
runs no consensus rule on its data: the Validated Header tree anchors at a trusted validator's
tip − depth (no history below it, no store), a header enters on its parent link and nBits work
alone, and bulk sync fetches
blocks by height checked only for integrity (parent link, merkle root). The consensus rules
(`zaino_header_chain::validate`) are for an untrusted source.

```text
                             zebrad (JSON-RPC)
                                    ▲
                  ┌─────────────────┴─────────────────┐
                  │ zaino-traffic   TrafficBalancer   │  who answers; hedge, retry, bench
                  └──┬──────────────┬──────────────┬──┘
           headers() │  block_at(h) │      block() │
                     ▼              │              │
            ┌──────────────────┐    │              │
            │ zaino-chainview  │    │              │
            │ header sync      │    │              │
            └────────┬─────────┘    │              │
                     │ watch<VerifiedChain>        │
          ┌──────────┴────────────┐ │              │
          ▼                       ▼ ▼              ▼
  ┌────────────────────┐   ┌────────────────────────────┐
  │ zaino-sync         │   │ zaino-nfs                  │
  │ FinalFollower      │   │ tip overlay (RAM)          │
  │ ≤ final tip        │   │ > each index's durable tip │
  └─────────┬──────────┘   └──────────────▲─────┬───────┘
            │ IndexerDataSink<Block>      │     │ Published (watch<Indexed>)
            ▼                             │     ▼
  ┌────────────────────┐ committed views  │  ┌──────────────────┐
  │ 5 index writers    │──────────────────┘  │ zaino-snapshot   │◀── ChainView (mempool)
  │ fold → commit      │                     │ one Snapshot     │
  └────────────────────┘                     └────────┬─────────┘
                                                      ▼
                                             zaino-grpc, /statusz, /metrics
```

## Where data lives

| Data                    | Where                    | Written by       | Read by               |
| ----------------------- | ------------------------ | ---------------- | --------------------- |
| headers near the tip    | `HeaderChain` (RAM)      | header sync      | `VerifiedChain` users |
| final blocks in flight  | sink queues (RAM)        | `FinalFollower`  | each writer           |
| index data, final       | one store / index (disk) | its writer       | snapshots             |
| index data, not durable | NFS graph (RAM, `imbl`)  | NFS folds        | snapshots             |
| what a request reads    | `Snapshot` (`ArcSwap`)   | `zaino-snapshot` | gRPC, status, metrics |

## Public interfaces

```rust
// zaino-chainview: the one answer to "what is the chain, and what is final"
HeaderSync::subscribe(&self) -> watch::Receiver<Option<Arc<VerifiedChain>>>;
VerifiedChain::{best(), final_tip(), on_best(at), hash_at(h), header_at(h), holds(at), forks()};

// zaino-traffic: every request to a validator
TrafficBalancer::block(&self, hash, Urgency) -> Answered<Block>;    // Urgency = Tip | Bulk
TrafficBalancer::block_at(&self, height, Urgency) -> Answered<Block>; // trusted members only
TrafficBalancer::report(&self, ticket, why);                        // wrong answer: bench sender

// zaino-sync: the final path (data-sink.md)
FinalFollower::new(chain, balancer, lookahead);
FinalFollower::subscribe(&mut self, kind, durable: Option<BlockRef>, queue) -> Subscription<Block>;
FinalFollower::run(self, cancel) -> Result<(), FollowError>;
fetch_at(balancer, height, Urgency) -> Checked;  // final path: coinbase height + merkle root
fetch(balancer, at, record, Urgency) -> Checked; // tip path: hash + height + merkle root
IndexHandle::{view() -> V, tip(), changed()};       // one index's committed view

// each index crate: one fold, one writer (data-sink.md)
fold(parent: &XReader<V>, block, [inputs,] out: &mut BlockChanges);      // shared by writer + NFS
XIndexWriter::{new(store, batch), handle() -> IndexHandle<V>, run(blocks)};

// zaino-nfs: the tip path (nfs.md)
Nfs::new(chain, balancer, params, depth, lookahead);                // window = 2 · depth
Nfs::add(&mut self, kind, index: IndexHandle<V>);
Nfs::indexed(&self) -> Published<V>;
Nfs::run(self, cancel) -> Result<(), NfsError>;

// zaino-snapshot: what every request pins
Snapshots::load(&self) -> Arc<Snapshot<V>>;
Snapshot::{served() -> Result<&At<V>, Unavailable>, mempool(), lightd()};
```

## Rules

1. **One fold per index, two callers.** The writer and the NFS run the same `fold`. A tip block is
   folded twice, once in RAM and once by its writer when it turns final. That costs about 1 ms
   per block, and it keeps the stream a plain `Block`.
2. **No serving state.** gRPC binds at boot and every endpoint answers at one snapshot tip: the
   lowest durable tip during bulk sync, best once the NFS folds (lowest durable tip within
   `2 · depth` of best). Every enabled index is readable at that tip; absent = disabled.
3. **The writers never see a reorg.** Reorgs live in the NFS graph alone.
4. **A request reads one `Snapshot`.** No height named = its tip; a height named = up to that
   index's own durable tip (`At::answers_through`).

Details: [data-sink.md](data-sink.md) (final path), [nfs.md](nfs.md) (tip path),
[global-snapshot.md](global-snapshot.md) (snapshot), [traffic-balancer.md](traffic-balancer.md)
(requests), `zaino-chainview` / `zaino-header-chain` usage guides (headers and finality).
