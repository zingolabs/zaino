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
// `view.validate_transaction(raw)` → `view.broadcast_transaction(validated)` sends;
// `view.subscriber()` = the read handle
let _ = (view, headers, merge);
# Ok(())
# }
```

## In `zainod`

zainod spawns the balancer's driver first, builds the view once the upgrade schedule is known
(regtest's = the first answered poll), then spawns header sync, the observation merge, each push
stream, and with `[p2p]` on `peer_watch()` and `peer_mempool(..)`. `zaino-grpc`'s `RouteDeps`
holds the view for `SendTransaction`; every read goes through `zaino-grpc-snapshot`. Each trusted
member is named by its `TrustedValidator::address` (`TrafficBalancer::addresses()`); no validator
outvotes another.

## Structure

```text
HeaderSync             trusted headers(Pinned) + peer headers(Peers) → HeaderChain → HeaderChainSnapshot
ObservationMerge       each trusted validator's poll (TrafficBalancer::observe): claim + getblockhash first,
      │                then the whole listing (bytes(..) fetched between)
PeerMempool / PeerWatch  peers' listings (quorum) / block invs (peer witnesses)
ChainViewCore          merges all of it into one ChainViewSnapshot via ArcSwap
ChainViewSubscriber    readers pin one `ChainViewSnapshot` per request
```

A listing is diffed against that validator's bits in the held sightings, under the merge lock;
only a change touches a sighting. Raw bytes are fetched once per txid, the lister preferred. Each
`MempoolEntry` carries a `Projection` (the serving layer's encoding), rendered once
(`get_or_render`); a fee arriving for an unpriced entry starts a fresh one.

## `HeaderSync`

`HeaderSync::run(cancel)` owns the `HeaderChain` (no lock). `subscribe()` (take it before `run`) =
`watch<Option<Arc<HeaderChainSnapshot>>>`, republished on every change (`HeaderChain::revision`):
a header, a witness, finality.

- **Trusted runs** (`headers(Pinned)`, ≤ `HEADER_BATCH` = 2,000): from the first answering
  validator whose claim the header chain does not hold. Anchors at claim − `anchor_below()`
  (max(depth, 113)) with no chain or that anchor past `ceiling` (an anchor header refused for its
  nBits = `report` + stall, nothing dropped); otherwise extends. The run's last
  header is vouched.
- **Own anchor** (`with_own_chain(OwnChain { locator, anchor, above })`, `with_peers`): no trusted
  run due before any anchor (a restart while every validator is down) → the peers' headers after
  `locator`, each matched to `anchor`'s hash then `above`'s in turn, anchor at `anchor` and fill
  the tree as trusted (our indexes folded those blocks). zainod builds it from tree-state's records:
  `above` = `CONTEXT` blocks ending at the indexes' lowest durable tip. A mismatch or a short answer
  is asked again after 5 s.
- **Peer runs** (`with_peers`, once anchored): `headers(Peers)` when no trusted run is due, every
  5 s (at once after progress), each header through `insert_untrusted` (`validate`). Never
  vouched. A refusal ends the run, and only `Fault::Sender` → `report`.
- **Witnesses** after every view change, then finality: each answering validator's claim and
  `getblockhash` answer (`Witness::Trusted`), each live outbound peer's last block `inv`
  (`PeerSource::heard` via `PeerWatch`: `Witness::Peer` by its netgroup). A hash not held yet is
  witnessed on a later pass.
- **Stall** (fetch failed, retreated, malformed, empty, refused, or a fully read branch still not
  holding the claim): that validator alone backs off 5 s. A trusted run orphaned at or below the
  floor marks the member `forked` for that claim: not fetched again until the claim moves.

## Read handle: `ChainViewSubscriber`

Cheap to clone; cannot drive polling or submit.

- `current()` → `Arc<ChainViewSnapshot>`: pin once per request.
- `subscribe_published()` → `watch::Receiver<()>`, changed on every publish (stored in merge order).

`ChainViewSnapshot`:

| Method | Answer |
|---|---|
| `chain()` | the `HeaderChainSnapshot` every standing was judged under |
| `best()` | the header chain's best, held or not |
| `held_by()` | `TrustedValidatorSet` of answering validators (`Live` or catching up) whose claim = best, or above it with their `getblockhash` answer on our chain: where each is now, read from its last poll |
| `agreements()` | each trusted validator's `Agreement`, configured order |
| `claimed_past_best()` | an answering validator claims above best, not shown off our chain (its hash at best not polled yet counts): best ≠ the network's tip yet |
| `mempool()` | `Some` iff a `Live` validator is in `held_by()` or the fresh peer listings span > 5 netgroups (needs no header chain) |
| `arrivals(since)` | transactions held here but not in `since` (`None` = all): an `imbl` diff |
| `mempool_states()` | each held transaction's `MempoolState`: `Verified` (a trusted listing), `Quorum` (a peer quorum's only), `Ours` (our relay, neither) |
| `trusted_validators()` | per-validator `TrustedValidatorMeta`, configured order |
| `alarms()` | below |

`TrustedValidatorSet`, `TrustedValidatorMeta` and `Agreement` are documented on their types.

## Validator states in the merge

| Poll result | Health | Claim | Sightings |
|---|---|---|---|
| answered, mempool listed | `Live` | held | kept |
| answered, mempool inactive ("mempool is not active") | `CatchingUp` | feeds header sync and vouching, holds no mempool | retracted |
| failed | `Degraded` | last one shown, holds nothing | kept |
| 10 failures | `Down` | cleared | retracted; next answer re-lists everything |

Health is the balancer's (`Observation::health`, mirrored into `TrustedValidatorMeta`), and so are
its transition logs. A validator going away never ends `run`; only cancel does.

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
- `get(txid)` = one entry by txid (`GetTransaction`'s mempool answer), no scan.
- `MempoolEntry.fee: Option<Zatoshis>`: the first known (a trusted listing's, or computed for a
  quorum admission); `None` = an `ours` nothing has listed.
- `ChainView::peer_mempool(utxos)` → `PeerMempool` to spawn. `PEER_MEMPOOL_POLL` = 5 s,
  `LISTING_FRESH` = 15 s (a failed answer keeps the peer's last listing while fresh),
  `REFUSED_TTL` = 10 min (only `UnknownPrevout` retries). Admission checks: `admission::admissible`.
- Held-output prevouts do not check whether another held transaction already spends them.

## Submission

Two steps, both `Result<_, SubmitError>`; the protocol is
[architecture §6](../../docs/architecture.md#6-submission).

- `ChainView::validate_transaction(raw)` → `ValidatedTransaction`: precheck + check below,
  nothing relayed (a refusal never leaves Zaino)
- `ChainView::broadcast_transaction(validated)` → `TxId`: the relay job; only a
  `ValidatedTransaction` goes in, so no relay skips the checks

- Precheck: expiry 0 = none; expiry and branch are judged only once a verified best exists.
- Check: `checkadmission` (a `submit` ask) on `entries()` ∩ `held_by()`, shuffled.
- `SubmitError::Rejected` = the precheck's or the check's refusal, else the first trusted
  rejection; `SubmitError::Unreachable { attempted, cause }` = never accepted, no rejection
  (cause = the last push failure).
- Job inputs, taken once at its start: peers = `PeerSource::live()` non-empty, trusted =
  `TrafficBalancer::entries()` (`Live`, not benched), checked = the check admitted it. A trusted
  entry's own listing counts for acceptance, not for spread.
- Job end: `spread`, `unconfirmed`, `rejected`, `unreachable`. Nothing ties it to mining or expiry.
- `SubmitPolicy { propagation_threshold, max_attempts }` is zainod's `[submission]` table as
  parsed (unknown keys refused; defaults in the example config).

## Ports

- Validators: members of a `zaino_traffic::TrafficBalancer<S: ChainDataSource>`. The view merges
  their polls (`ObservationMerge`), asks `bytes(..)` and `submit(validator, ..)`
  (`checkadmission`, `sendrawtransaction`); header sync asks `headers(..)`.
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

This crate emits events only (its `METRICS`: submissions by outcome); the `zaino_chainview_*`
gauges are `zaino-grpc-snapshot`'s, set at scrape.

`alarms()` → `Alarms`: each raised `Alarm` (`FinalityPaused`, and per validator `Stale`,
`Ending`), derived per merge ([architecture §11](../../docs/architecture.md#11-telemetry);
thresholds in `alarm_thresholds.rs`). A new alarm = an `Alarm` variant: the compiler then names
its arms (raised in `telemetry::alarms`, noted and logged in `zaino-grpc-snapshot`'s report).

## Testing (feature `testing`)

- `ChainView::set_chain(Some(HeaderChainSnapshot::of(&chain, tip)))` stands in for header sync
  (`zaino-header-chain`'s `testing` feature).
- `ChainViewSnapshot::fixed(chain, held_by, addresses, ours)`: one view, no `ChainView` (each
  holder `Live` at best); `.ahead(at, by, read)`: holder `at` claims `by` blocks past best
  (`read` = its hash at best polled: `Ahead`; else `Diverged`, no longer a holder);
  `.trusted_validator(at, health, behind)`, `.peers_listed(peers)`.

## Tests

- `mock_peers::MockPeers` (`PeerSource` as a script), beside
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

- `mempool_model_tests.rs`: the quorum against a naive oracle. Guarantee: rivals never both held
  with every peer honest; newly both held only while a fresh peer lists both.
