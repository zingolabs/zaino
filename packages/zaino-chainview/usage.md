# `zaino-chainview` — usage

Header sync, the mempool view and submission over N trusted validators and, optionally, peers.
What each source is trusted for and why: [architecture §3–§6](../../docs/architecture.md#3-sources-and-what-each-is-trusted-for).

The view keeps no durable state: a restart loses nothing one poll round does not restore. Every
request goes through one `zaino_traffic::TrafficBalancer`.

```rust
use zaino_chainview::{ChainView, SubmitPolicy};
use zaino_header_chain::HeaderChain;
use zaino_primitives::protocol::compiled_schedule;
use zaino_primitives::types::FinalityDepth;
use zaino_traffic::TrafficBalancer;
use zcash_protocol::consensus::NetworkType;

# fn wire<S: zaino_source::ChainDataSource>(balancer: TrafficBalancer<S>, chain: HeaderChain, shutdown: tokio_util::sync::CancellationToken) -> Result<(), Box<dyn std::error::Error>> {
// - trusted validators named by `balancer.addresses()` (logs, status)
// - depth = the header chain's finality depth (zainod passes `sync.finalised_depth`)
// - schedule = the network's (branch a transaction is signed for at the next block, regtest's
//   = its validator's)
let schedule = compiled_schedule(NetworkType::Main).ok_or("mainnet compiled in")?;
let view = ChainView::new(balancer, FinalityDepth::CONSENSUS, schedule, shutdown.child_token());
let view = view.with_submit_policy(SubmitPolicy::default());
// the tip comes only from header sync, the polls from the merge (the balancer's driver runs
// beside them)
let headers = view.header_sync(chain);
let merge = view.observation_merge();
// each: `tokio::spawn(x.run(cancel.child_token()))`
// `view.submit(raw)` submits; `view.subscriber()` = the read handle
let _ = (view, headers, merge);
# Ok(())
# }
```

## In `zainod`

zainod spawns the balancer's driver first, builds the view once the upgrade schedule is known
(regtest's = the first answered poll), then spawns header sync, the observation merge, each push
stream (`indexer_address` set: callbacks → `TrafficBalancer::pushed`), and with `[p2p]` on
`peer_watch()` and `peer_mempool(ServedUtxos)`. `zaino-grpc`'s `RouteDeps.submit` holds the view for
`SendTransaction`; every read goes through `zaino-grpc-snapshot`.

## Membership

- `ChainView::new` names each trusted member by its `TrustedValidator::address`
  (`TrafficBalancer::addresses()`). The balancer itself holds 1 ..= `TrustedValidatorId::MAX` = 64
  members; zainod refuses a config outside that before boot.
- No validator outvotes another. Each contributes headers, `getblockhash` answers saying which
  verified blocks it holds, a mempool listing and a submission entry.

## Structure

```text
HeaderSync             trusted headers(Pinned) + peer headers(Peers) → HeaderChain → HeaderChainSnapshot
ObservationMerge       each trusted validator's poll (TrafficBalancer::observe): claim + getblockhash first,
      │                then the listing diff (bytes(..) fetched between)
PeerMempool / PeerWatch  peers' listings (quorum) / block invs (peer finality)
ChainViewCore          merges all of it into one ChainViewSnapshot via ArcSwap
ChainViewSubscriber    readers pin one `ChainViewSnapshot` per request
```

The merge is `O(change)`. Raw bytes are fetched once per txid, the lister preferred. Each
`MempoolEntry` carries a `Projection` (the serving layer's encoding), rendered once
(`get_or_render`); a fee arriving for an unpriced entry starts a fresh one.

## `HeaderSync`

`HeaderSync::run(cancel)` owns the `HeaderChain` (no lock). `subscribe()` (take it before `run`) =
`watch<Option<Arc<HeaderChainSnapshot>>>`, republished whenever best or finalized moves.

- **Trusted runs** (`headers(Pinned)`, ≤ `HEADER_BATCH` = 2,000): from the first answering
  validator whose claim the header chain does not hold. Anchors at claim − `anchor_below()`
  (max(depth, 113)) with no chain or that anchor past `ceiling`; otherwise extends. The run's last
  header is vouched.
- **Peer runs** (`with_peers`, once anchored): `headers(Peers)` when no trusted run is due, every
  5 s (at once after progress), each header through `insert_untrusted` (`validate`). Never
  vouched. `FromTheFuture`, `Orphan`, `BelowFinalized` and `ShortContext` end the run unblamed; any
  other refusal → `report`.
- **Vouches** after every poll merge: each answering validator's claim and its held `getblockhash`
  answer.
- **Stall** (fetch failed, retreated, malformed, empty, refused, or a fully read branch still not
  holding the claim): that validator alone backs off 5 s. A trusted run orphaned at or below the
  floor marks the member `forked` for that claim: not fetched again until the claim moves.
- **Peer finality** (`with_peers`): `peer_finalizable()` + live outbound peers in > 5 netgroups
  whose last block `inv` (`PeerSource::heard` via `PeerWatch`) is on our best at or above it.

## Read handle: `ChainViewSubscriber`

Cheap to clone; cannot drive polling or submit.

- `current()` → `Arc<ChainViewSnapshot>`: pin once per request.
- `subscribe_published()` → `watch::Receiver<()>`, changed on every publish (stored in merge order).

`ChainViewSnapshot`:

| Method | Answer |
|---|---|
| `chain()` | the `HeaderChainSnapshot` every standing was judged under |
| `best()` | the verified best, held or not |
| `held_by()` | `TrustedValidatorSet` of answering validators (`Live` or catching up) whose claim = best, or above it with their `getblockhash` answer on our chain |
| `claimed_past_best()` | an answering validator claims above best, not shown off our chain (its hash at best not polled yet counts): best ≠ the network's tip yet |
| `mempool()` | `Some` iff a `Live` validator is in `held_by()` or `peers_quorate()` |
| `peers_quorate()` | the fresh peer listings span > 5 netgroups (needs no verified chain) |
| `arrivals(since)` | transactions held here but not in `since` (`None` = all): an `imbl` diff |
| `mempool_states()` | each held transaction's `MempoolState`: `Verified` (a trusted listing), `Quorum` (a peer quorum's only), `Ours` (our relay, neither) |
| `trusted_validators()` | per-validator `TrustedValidatorMeta`, configured order |
| `alarms()`, `shared_outbound_min()` | below |

`TrustedValidatorSet` is a bitset over `TrustedValidatorId`s: `positions()` yields configured
positions, `TrustedValidatorSet::at(positions)` builds one (`testing` feature).

`TrustedValidatorMeta`: address, `tip()`, `zaino_traffic::Health` as of its last poll, `Agreement`,
peers (every 60 s), release (build, user agent, end-of-service
height), push-stream state, `blocks_to_end_of_service()`.

`Agreement` from its claim and `getblockhash` answer, as of its last answered poll: `Agreed` (claim
= best), `Ahead` (claims higher, answer on our chain), `Behind` (claim on our chain, below best),
`Diverged` (none of these: telemetry, never an error), `Unknown` (nothing verified, or last poll
failed).

## Validator states in the merge

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

- Precheck: undecodable → `Rejected` code −22; once a verified best exists, expired
  (`next height > expiry`, expiry 0 = none) or wrong v5+ branch → `Rejected` code −25.
- Check: `TrafficBalancer::check_admission` on `entries()` ∩ `held_by()`, shuffled; the first
  `CheckAdmissionError::Refused` = `SubmitError::Rejected` at once (outcome `rejected`, 0
  attempts), the first `Ok` = the answer; `Unsupported` / no answer = the next.
- `SubmitError::Rejected` = the precheck's or the check's refusal, else the first trusted rejection;
  `SubmitError::Unreachable { attempted, cause }` = never accepted, no rejection (cause = the last
  push failure).
- Job inputs, taken once at its start: peers = `PeerSource::live()` non-empty, trusted =
  `TrafficBalancer::entries()` (`Live`, not benched), checked = the check admitted it.
- A trusted entry's own listing counts for acceptance, not for spread.
- Job end: `spread`, `unconfirmed`, `rejected`, `unreachable`. Nothing ties it to mining or expiry.
- `SubmitPolicy { propagation_threshold, max_attempts }` = 15 s, 4 by default. It is zainod's
  `[submission]` table as parsed (`Deserialize`/`Serialize`: `propagation_threshold_secs`,
  non-zero whole seconds, and `max_attempts`; unknown keys refused).

## Ports

- Validators: members of a `zaino_traffic::TrafficBalancer<S: ChainDataSource>`. The view merges
  their polls (`observe`), asks `bytes(..)`, `check_admission(..)` and `submit(..)`; header sync
  asks `headers(..)`.
  `ChainView::new` wires `poll_best` (each poll's `getblockhash` at our best).
- `PeerSource` (`dyn`, given by `with_peers`):

| Method | Answer |
|---|---|
| `heard()` | a fresh stream of every block `inv` (`zaino_primitives::types::Announced { peer, hash }`) |
| `live()` | connected outbound peers (each one's mempool polled) |
| `push(raw)` | one relay to ~⅓ of them, unattributed; `Ok` = delivered (a peer answers nothing) |
| `mempool(peer)` | `peer`'s whole mempool, that peer alone answering (`PeerTxId`s) |
| `transactions(ids)` | each id's bytes in order (`None` = no peer supplied it) |

- `UnspentOutputs::values(outpoints)`: the transparent UTXO set at the served tip (`None` = none
  yet).
- `ChainView::peer_watch()` → `PeerWatch`: merges block `inv`s every 250 ms.

## Telemetry

zainod registers descriptions via `describe_metrics()`. The `zaino.chainview.*` gauges are
`zaino-grpc-snapshot`'s, set at scrape. This crate emits events only (buckets: `METRIC_BUCKETS`):

| Metric | Kind | Labels |
|---|---|---|
| `first_trusted_seconds` | histogram | `origin` (`ours`, `network`) |
| `all_trusted_seconds` | histogram | |
| `residence_seconds` | histogram | `left` (`block`, `unlisted`), `origin` |
| `submission_attempts` | histogram | `outcome` |
| `submissions_total` | counter | `outcome` (`spread`, `unconfirmed`, `rejected`, `unreachable`) |

`alarms()` → `Alarms { stale, ending, partitioned, eclipsed, finality_paused }`, raised per merge
(definitions and thresholds: [architecture §11](../../docs/architecture.md#11-telemetry); in
`config.rs`).

## Testing (feature `testing`)

- `ChainView::set_chain(Some(chain.snapshot(tip)))` stands in for header sync
  (`zaino_header_chain::testing::HeaderViews`).
- `ChainViewSnapshot::fixed(chain, held_by, addresses, ours)`: one view, no `ChainView` (each
  holder `Live` + `Agreed`); `.ahead(at, by, read)`: holder `at` claims `by` blocks past best
  (`read` = its hash at best polled: `Ahead`; else `Diverged`, no longer a holder).
- `testing::MockPeers` (`PeerSource` as a script), beside
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
