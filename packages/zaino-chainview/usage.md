# `zaino-chainview` — usage

Header sync, the mempool view and submission over N trusted validators and, optionally, peers.
What each source is trusted for and why: [architecture §3–§6](../../docs/architecture.md#3-sources-and-what-each-is-trusted-for).

The view keeps no durable state: a restart loses nothing one poll round does not restore. Every
request goes through one `zaino_traffic::TrafficBalancer`.

```rust
use zaino_chainview::{ChainView, SubmitPolicy};
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::{ReorgDepth, UpgradeSchedule};
use zaino_traffic::TrafficBalancer;
use zcash_protocol::consensus::NetworkType;

# fn wire<S: zaino_source::ChainDataSource>(balancer: TrafficBalancer<S>, chain: HeaderChain) -> Result<(), Box<dyn std::error::Error>> {
// addresses[i] = the balancer's trusted member i (logs, status); depth = the header chain's
// finality depth (zainod passes `sync.finalised_depth`); schedule = the network's (the branch
// a transaction must be signed for at the next block; regtest's = its validator's)
let addresses = vec!["127.0.0.1:8232".to_owned()];
let schedule = UpgradeSchedule::compiled(NetworkType::Main).ok_or("mainnet compiled in")?;
let view = ChainView::new(addresses, balancer, ReorgDepth::CONSENSUS, schedule)?;
let view = view.with_submit_policy(SubmitPolicy::default());
// the tip comes only from header sync, the polls from the fold (the balancer's driver runs
// beside them)
let headers = view.header_sync(chain);
let fold = view.observation_fold();
// each: `tokio::spawn(x.run(cancel.child_token()))`
// `view.submit(raw)` submits; `view.subscriber()` = the read handle
let _ = (view, headers, fold);
# Ok(())
# }
```

## In `zainod`

zainod spawns the balancer's driver first, builds the view once the upgrade schedule is known
(regtest's = the first answered poll), then spawns header sync, the observation fold, each push
stream (`indexer_address` set: callbacks → `TrafficBalancer::pushed`), and with `[p2p]` on
`peer_watch()` and `peer_mempool(ServedUtxos)`. `zaino-grpc`'s `Routes.submit` holds the view for
`SendTransaction`; every read goes through `zaino-snapshot`.

## Membership

- `ChainView::new` rejects an empty list (`ConfigError::NoEndpoints`) and more than
  `ValidatorId::MAX` = 64 (`ConfigError::TooManyEndpoints`). One address per trusted member of the
  balancer (`ConfigError::AddressCountMismatch` otherwise).
- No validator outvotes another. Each contributes headers, `getblockhash` answers saying which
  verified blocks it holds, a mempool listing and a submission entry.

## Structure

```text
HeaderSync             trusted headers(Pinned) + peer headers(Peers) → HeaderChain → VerifiedChain
ObservationFold        each member's poll (TrafficBalancer::observe): claim + getblockhash first,
      │                then the listing diff (bytes(..) fetched between)
PeerMempool / PeerWatch  peers' listings (quorum) / block invs (peer finality)
ChainViewCore          folds all of it into one ChainViewSnapshot via ArcSwap
ChainViewSubscriber    readers pin one snapshot per request
```

The fold is `O(change)`. Raw bytes are fetched once per txid, the lister preferred. Each
`MempoolEntry` carries a `Projection` (the serving layer's encoding), rendered once
(`get_or_render`); a fee arriving for an unpriced entry starts a fresh one.

## `HeaderSync`

`HeaderSync::run(cancel)` owns the `HeaderChain` (no lock). `subscribe()` (take it before `run`) =
`watch<Option<Arc<VerifiedChain>>>`, republished whenever best or final moves.

- **Trusted runs** (`headers(Pinned)`, ≤ `HEADER_BATCH` = 2,000): from a validator whose claim is off
  our best. Anchors at claim − `anchor_below()` (max(depth, 113)) with no chain or a claim past
  `ceiling`; otherwise extends. The run's last header is vouched.
- **Peer runs** (`with_peers`, once anchored): `headers(Peers)` every 5 s when no trusted run is
  due, each header through `insert_untrusted` (`validate`). Never vouched. `FromTheFuture`,
  `Orphan`, `BelowFinal` and `ShortContext` end the run unblamed; any other refusal → `report`.
- **Vouches** after every poll fold: each answering validator's claim and its held `getblockhash`
  answer.
- **Stall** (fetch failed, malformed, empty, refused, or a fully read branch still not holding the
  claim): that validator alone backs off 5 s. A trusted run orphaned at or below the floor marks
  the member `forked` for that claim: not fetched again until the claim moves.
- **Peer finality** (`with_peers`): `peer_finalizable()` + live outbound peers in > 5 netgroups
  whose last block `inv` (`Heard::Block` via `PeerWatch`) is on our best at or above it.

## Read handle: `ChainViewSubscriber`

Cheap to clone; cannot drive polling or submit.

- `current()` → `Arc<ChainViewSnapshot>`: pin once per request.
- `subscribe_published()` → `watch::Receiver<()>`, changed on every publish (stored in fold order).

`ChainViewSnapshot`:

| Method | Answer |
|---|---|
| `chain()` | the `VerifiedChain` every standing was judged under |
| `best()` | the verified best, held or not |
| `held_by()` | `EndpointSet` of answering validators (`Live` or catching up) whose claim = best, or above it with their `getblockhash` answer on our chain |
| `claimed_past_best()` | an answering validator claims above best, not shown off our chain (its hash at best not polled yet counts): best ≠ the network's tip yet |
| `mempool()` | `Some` iff a `Live` validator is in `held_by()` or `peers_quorate()` |
| `peers_quorate()` | the fresh peer listings span > 5 netgroups (needs no verified chain) |
| `arrivals(since)` | transactions held here but not in `since` (`None` = all): an `imbl` diff |
| `spread(txid)` / `spreads()` | `Spread { peers: Count, trusted: Count, quorum, ours }` |
| `endpoints()` | per-endpoint `ValidatorMetadata`, configured order |
| `alarms()`, `shared_outbound_min()` | below |

`EndpointSet` is a bitset over `ValidatorId`s: `positions()` yields configured positions,
`EndpointSet::at(positions)` builds one (`testing` feature).

`ValidatorMetadata`: address, `tip()`, `zaino_traffic::Health` as of its last poll, `Agreement`,
last-observed time, peers (every 60 s), release (build, user agent, protocol, end-of-service
height), push-stream state, `blocks_to_end_of_service()`. Latency and failures are the balancer's
(`TrafficBalancer::members()`).

`Agreement` from its claim and `getblockhash` answer, as of its last answered poll: `Agreed` (claim
= best), `Ahead` (claims higher, answer on our chain), `Behind` (claim on our chain, below best),
`Diverged` (none of these: telemetry, never an error), `Unknown` (nothing verified, or last poll
failed).

## Validator states in the fold

| Poll result | Health | Claim | Sightings |
|---|---|---|---|
| answered, mempool listed | `Live` | held | kept |
| answered, mempool inactive ("mempool is not active") | `CatchingUp` | feeds header sync and vouching, holds no mempool | retracted |
| failed | `Degraded` | last one shown, holds nothing | kept (still in `trusted.seen`, not in `of`) |
| 10 failures | `Down` | cleared | retracted; next answer re-lists everything |

INFO on a validator's first listing; WARN every 60 s while catching up, then "Validator caught
up". A validator going away never ends `run`; only cancel does.

## Mempool

```rust
# use zaino_chainview::ChainViewSubscriber;
# fn serve(view: &ChainViewSubscriber, exclude: Vec<Vec<u8>>) -> Result<(), Box<dyn std::error::Error>> {
let pinned = view.current();
let entries = pinned.mempool().ok_or("no holder: UNAVAILABLE")?.excluding(&exclude);
# let _ = entries;
# Ok(())
# }
```

- `excluding(suffixes)` = `GetMempoolTx`'s rule: a suffix matches the end of the txid's
  protocol-order bytes; a suffix matching 0 or ≥ 2 entries excludes nothing.
- `MempoolEntry.fee: Option<Zatoshis>`: the first known (a trusted listing's, or computed for a
  quorum admission); `None` = an `ours` nothing has listed.
- `ChainView::peer_mempool(utxos)` → `PeerMempool` to spawn. `PEER_MEMPOOL_POLL` = 5 s,
  `LISTING_FRESH` = 15 s (a failed answer keeps the peer's last listing while fresh),
  `REFUSED_TTL` = 10 min (only `UnknownPrevout` retries). Admission checks: `admission::admissible`.
- Held-output prevouts do not check whether another held transaction already spends them.

## Submission

`ChainView::submit(raw)` → `Result<TxId, SubmitError>`; the protocol is
[architecture §6](../../docs/architecture.md#6-submission).

- Precheck (skipped before a verified best exists): undecodable → `Rejected` code −22; expired
  (`next height > expiry`, expiry 0 = none) or wrong v5+ branch → `Rejected` code −25.
- Check: `TrafficBalancer::check_admission` on `entries()` ∩ `held_by()`, shuffled; the first
  `CheckAdmissionError::Refused` = `SubmitError::Rejected` at once (outcome `rejected`, 0
  attempts), the first `Ok` = the answer; `Unsupported` / no answer = the next.
- `SubmitError::Rejected` = the check's refusal, else the first trusted rejection;
  `SubmitError::Unreachable { attempted, cause }` = never accepted, no rejection (cause = the last
  push failure).
- Job inputs, taken once at its start: peers = `ValidatorP2pSource::live()` non-empty, trusted =
  `TrafficBalancer::entries()` (`Live`, not benched), checked = the check admitted it.
- A trusted entry's own listing counts for acceptance, not for spread.
- Job end: `spread`, `unconfirmed`, `rejected`, `unreachable`. Nothing ties it to mining or expiry.
- `SubmitPolicy { propagation_threshold, max_attempts }` = 15 s, 4; zainod sets it from
  `[submission]`.

## Ports

- Validators: members of a `zaino_traffic::TrafficBalancer<S: ChainDataSource>`. The view folds
  their polls (`observe`), asks `bytes(..)`, `check_admission(..)` and `submit(..)`; header sync
  asks `headers(..)`.
  `ChainView::new` wires `poll_best` (each poll's `getblockhash` at our best).
- `ValidatorP2pSource` (`dyn`, given by `with_peers`):

| Method | Answer |
|---|---|
| `heard()` | a fresh stream of every block `inv` (`Heard { peer, hash }`) |
| `live()` | connected outbound peers (each one's mempool polled) |
| `push(raw)` | one relay to ~⅓ of them, unattributed; `Ok` = delivered (a peer answers nothing) |
| `mempool(peer)` | `peer`'s whole mempool, that peer alone answering (`PeerTxId`s) |
| `transactions(ids)` | each id's bytes in order (`None` = no peer supplied it) |

- `UnspentOutputs::values(outpoints)`: the transparent UTXO set at the served tip (`None` = none yet).
- `ChainView::peer_watch()` → `PeerWatch`: folds block `inv`s every 250 ms.

## Telemetry

zainod registers descriptions via `describe_metrics()`. The `zaino.chainview.*` gauges are
`zaino-snapshot`'s, set at scrape. This crate emits events only (buckets: `METRIC_BUCKETS`):

| Metric | Kind | Labels |
|---|---|---|
| `first_trusted_seconds` | histogram | `origin` (`ours`, `network`) |
| `all_trusted_seconds` | histogram | |
| `residence_seconds` | histogram | `left` (`block`, `unlisted`), `origin` |
| `submission_attempts` | histogram | `outcome` |
| `submissions_total` | counter | `outcome` (`spread`, `unconfirmed`, `rejected`, `unreachable`) |

`alarms()` → `Alarms { stale, ending, partitioned, eclipsed, finality_paused }`, raised per fold
(definitions: [architecture §11](../../docs/architecture.md#11-telemetry)). Constants:
`STALE_TIP_BLOCKS` = 24, `END_OF_SERVICE_WARN_BLOCKS` = 8,064, `ECLIPSE_OUTBOUND_MAX` = 2,
`FINALITY_STALL` = 60 s.

## Testing (feature `testing`)

- `ChainView::set_verified(Some(chain.verified(tip)))` stands in for header sync
  (`zaino_header_chain::testing::HeaderViews`).
- `ChainViewSnapshot::fixed(chain, held_by, addresses, ours)`: one view, no `ChainView` (each
  holder `Live` + `Agreed`); `.ahead(at, by, read)`: holder `at` claims `by` blocks past best
  (`read` = its hash at best polled: `Ahead`; else `Diverged`, no longer a holder).
- `testing::MockPeers` (`ValidatorP2pSource` as a script), beside
  `zaino_source::testing::MockValidator`; a `MockChain` tip's `Ledger` is the `UnspentOutputs`
  (fund prevouts by mining them, e.g. a coinbase with a stated txid):

```rust,ignore
let peers = Arc::new(MockPeers::new(live, dead));     // dead refuse a listing; no live = no relay
let view = view.with_peers(peers.clone());
let utxos = Arc::new(chain.ledger(tip));             // that tip's unspent outputs
tokio::spawn(view.peer_mempool(utxos).expect("peers").run(cancel));
peers.hold(id, raw);                                 // bytes served for `id`
peers.list(peer, vec![id]);                          // `peer`'s mempool from its next listing
peers.announce_block(peer, hash);                    // one block `inv` (after peer_watch subscribed)
let relays: usize = peers.pushes();                  // relays asked for so far
```

- `mempool_model.rs`: the quorum against a naive oracle. Guarantee: rivals never both held with
  every peer honest; newly both held only while a fresh peer lists both.
