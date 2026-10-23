# `zaino-peers` — usage

Zaino's p2p layer ([architecture §3](../../docs/architecture.md#3-sources-and-what-each-is-trusted-for)): zebra-network's peer set,
embedded. Peers are discovered and unauthenticated, so nothing here votes: every
header, block and transaction handed out is identified by what zebra recomputes
from its bytes, never by what a peer claimed.

```rust,ignore
use zaino_peers::{PeerConfig, PeerNetwork};

let peers = PeerNetwork::start(PeerConfig::new(NetworkType::Main)).await; // seeds dialed + crawled
let headers = peers.find_headers(&locator).await?;                       // raw header bytes
let block = peers.block_by_hash(hash).await?;                            // Option<Supplied>
let listed = peers.mempool_of(peers.live()[0]).await?;                   // that peer's whole mempool
let txs = peers.transactions_by_id(&listed).await?;                      // (id, Option<bytes>)
let mut heard = peers.announcements();                                   // Announced { peer, hash }
let txid = peers.push(&raw).await?;                                      // the bytes' own txid
```

## Dependency

zebra-network and zebra-chain come from the `elicbarbieri/zebra` fork (zebra main on
the same librustzcash, orchard and sapling-crypto forks the workspace patches, plus
`init`'s advertised services and `Request::MempoolTransactionIdsFrom`), pinned by rev in
the root `Cargo.toml`. zebra-network is built on tower 0.4, so this crate uses tower 0.4
for its peer set (`tower::buffer::Buffer`); nothing tower-typed is public.

zebra-network's codec parses in `block_in_place`: `PeerNetwork` needs a
multi-thread tokio runtime.

## Starting

`PeerNetwork::start(config)` wraps zebra-network's `init`:

- zebra always opens a listener: `127.0.0.1:0`, since Zaino serves no peer. The inbound service answers every request with nothing (`Nil`)
- `initial_peers` empty uses the network's DNS seeders; a list replaces them
  (regtest keeps loopback addresses only, as zebrad does)
- `peer_target` (`DEFAULT_PEER_TARGET`, 8) is zebra's initial outbound target. Each
  peer holds one inbound slot on a public node, so keep it modest; `request_timeout`
  (20 s) bounds every request and every push
- `cache_dir = None` disables zebra's peer address cache

It returns once the seeds are dialed and crawled once (`init` awaits one `getaddr`:
seconds, up to zebra's crawl timeout for a seed that never answers).

Zaino advertises no services (`PeerServices::empty()` to `init`), so it never claims to
serve blocks, and answers peers' requests with nothing. Zebra still admits it:
they require `NODE_NETWORK` only from peers they dial themselves, and only while syncing.

## Requests

All go through zebra's peer set, which picks the peer (power of two choices over its
ready peers), except `mempool_of`, which names it, and `push`, which zebra sends to
several.

| Method | Request | Returns |
|---|---|---|
| `find_headers(known)` | `FindHeaders` | consensus bytes per header, best-chain order |
| `block_by_hash(hash)` | `BlocksByHash` | `Some(Supplied { bytes, peer })` / `None` (not supplied) |
| `mempool_of(peer)` | `MempoolTransactionIdsFrom(peer)` | that peer's whole mempool: `PeerTxId`s (txid + v5 auth digest) |
| `transactions_by_id(ids)` | `TransactionsById` | `(id, Some(bytes) / None)` in request order |
| `push(raw)` | `PushTransaction` | the bytes' own txid |

- `mempool_of(peer)`: that peer alone answers (`mempool` → `inv`); a peer not ready
  (busy, gone, never connected) = `PeerError::Network`, never another peer's listing.
  An `inv` announcement landing before the reply is taken as the reply (the protocol
  cannot tell them apart): one short listing, the next poll's whole. zebrad answers
  an empty mempool with nothing: an empty listing.

- A block or transaction whose recomputed id was not asked for is dropped and its
  sender scored through zebra's misbehaviour channel (its address book bans past the
  threshold).
- A peer that supplies nothing asked for ends zebra's request in `SharedPeerError`
  (`notfound`, or an unsolicited reply arriving first: zebra's `peer/connection.rs`).
  These methods read that as an answer, every item `None`, not a failure. An
  unsolicited transaction also ends a reply partway: items after it come back `None`
  and are asked again later.
- `PeerError::Timeout` / `Network` = the request failed (no ready peer, a transport
  error).
- `push(raw)`: parses `raw` (zebra's parse-time checks and point rules; `PushError::Wire`
  otherwise, nothing sent), then zebra relays it as zebrad relays its own: to ⌈ready/3⌉
  random ready peers (all of them on regtest), over the peer set's existing connections
  ([why, and what a relay proves](../../docs/architecture.md#6-submission)).
  `PushError::Peers` = no ready peer within `request_timeout`.

## Announcements: the chain each peer carries

`announcements()` subscribes to every block `inv` a connected peer sends, the
sender's address attached by zebra: `zaino_primitives::types::Announced { peer, hash }`, the peer's new tip
(one block per `inv`; zebra drops multi-block ones), which the chain view counts by
netgroup toward finality on peers' word.

Transaction `inv`s are ignored ([why](../../docs/architecture.md#5-the-mempool-view)).
Mempool membership is each peer's own listing (`mempool_of`).

A subscriber that falls 4,096 announcements behind gets `RecvError::Lagged` and
knows it missed some.

`live()`: outbound peers live in the last few minutes (the ones the chain view polls).

## Metrics

Every request above passes one function that times and counts it (`METRICS`). The latency
histogram holds answered requests only, so it is a network round trip and nothing else: the
wait for a ready peer, failures (zebra refuses many without touching the wire) and pushes (zebra
answers `Nil` itself) are not in it. Its `request` label is zebra's `Request::command`, as on
zebra's own `zebra_net_out_requests` and `zcash_net_out_messages`, so a latency lines up with its
message counts.

## Tests

- Loopback, no DNS: a scripted zebra-network regtest node (a full `init`: the peer set refuses
  isolated connections as outbound peers) serves real mainnet blocks 1–3 and three
  transactions, lying for one of them. Headers, blocks and pushes arrive byte-identical, the lie
  never labels the asked id, and announcements arrive attributed to their sender.
- Real mainnet blocks 1 and 1,687,107 round-trip through every wire conversion.
