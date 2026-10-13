# `zaino-peers` — usage

Zaino's p2p layer ([architecture §3](../../docs/architecture.md#3-sources-and-what-each-is-trusted-for)): zebra-network's peer set,
embedded. Peers are discovered and unauthenticated, so nothing here votes: every
header, block and transaction handed out is identified by what zebra recomputes
from its bytes, never by what a peer claimed.

```rust,ignore
use zaino_peers::{PeerConfig, PeerNetwork};

let peers = PeerNetwork::start(PeerConfig::new(NetworkType::Main)).await; // seeds dialed + crawled
let headers = peers.find_headers(&locator, None).await?;                 // raw header bytes
let blocks = peers.blocks_by_hash(&hashes).await?;                       // (hash, Option<bytes>)
let listed = peers.mempool_of(peers.live()[0]).await?;                   // that peer's whole mempool
let txs = peers.transactions_by_id(&listed).await?;                      // (id, Option<bytes>)
let mut heard = peers.announcements();                                   // Announced { peer, hash }
let txid = peers.push(&raw).await?;                                      // the bytes' own txid
```

## Dependency

zebra-network and zebra-chain come from the `elicbarbieri/zebra` fork
(`lazy-network-main-ff14`: zebra main on the same librustzcash, orchard and
sapling-crypto forks the workspace patches, plus `init`'s advertised services and
`Request::MempoolTransactionIdsFrom`), pinned by rev in the root `Cargo.toml`. zebra-network is built on tower 0.4, so this crate uses tower
0.4 for its peer set (`tower::buffer::Buffer`); nothing tower-typed is public.

zebra-network's codec parses in `block_in_place`: `PeerNetwork` needs a
multi-thread tokio runtime.

## Starting

`PeerNetwork::start(config)` wraps zebra-network's `init`:

- `listen_addr` defaults to `127.0.0.1:0`: zebra always opens a listener, and Zaino
  serves no peer. The inbound service answers every request with nothing (`Nil`)
- `initial_peers = None` uses the network's DNS seeders; `Some(list)` replaces them
  (regtest keeps loopback addresses only, as zebrad does)
- `peer_target` (`DEFAULT_PEER_TARGET`, 8) is zebra's initial outbound target. Each
  peer holds one inbound slot on a public node, so keep it modest; `request_timeout`
  (20 s) bounds every request and every push
- `cache_dir = None` disables zebra's peer address cache

It returns once the seeds are dialed and crawled once (`init` awaits one `getaddr`:
seconds, up to zebra's crawl timeout for a seed that never answers).

Zaino advertises no services (`PeerServices::empty()` to `init`), so it never claims to
serve blocks, and answers peers' requests with nothing. Zebra and Zakura still admit it:
they require `NODE_NETWORK` only from peers they dial themselves, and only while syncing.

## Requests

All go through zebra's peer set, which picks the peer (power of two choices over its
ready peers), except `mempool_of`, which names it, and `push`, which zebra sends to
several.

| Method | Request | Returns |
|---|---|---|
| `find_headers(known, stop)` | `FindHeaders` | consensus bytes per header, best-chain order |
| `blocks_by_hash(hashes)` | `BlocksByHash` | `(hash, Some(bytes) / None)` in request order |
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
  random ready peers (all of them on regtest), over the peer set's existing connections.
  A second connection from our IP would be dropped by a zebrad peer's one-per-IP limit.
  Zebra names no recipient and a peer answers a push with nothing, so this is delivery,
  not a verdict. `PushError::Peers` = no ready peer within `request_timeout`.

## Announcements: the chain each peer carries

`announcements()` subscribes to every block `inv` a connected peer sends, the
sender's address attached by zebra: `Announced { peer, hash }`, the peer's new tip
(one block per `inv`; zebra drops multi-block ones), which the chain view counts by
netgroup toward finality on peers' word.

Transaction `inv`s are ignored: zebrad and zakura announce each admitted
transaction to a third of their peers, once, so a peer's silence says nothing.
Mempool membership is each peer's own listing (`mempool_of`).

A subscriber that falls 4,096 announcements behind gets `RecvError::Lagged` and
knows it missed some.

`live()`: outbound peers live in the last few minutes (the ones the chain view polls).

## Tests

- Loopback, no DNS: a scripted zebra-network regtest node (a full `init`, since
  isolated connections advertise no services and the peer set refuses those as
  outbound peers) serves real mainnet blocks 1–3 and three real transactions, lying
  for one of them. It admits Zaino and records it with no services. Headers and blocks
  arrive byte-identical, its mempool asked by address lists its three ids (a stranger's
  = a failure), the lie never labels the asked id, a transaction `inv` is ignored and a
  block `inv` arrives attributed to the sender. A push lands byte-identical at a peer
  the peer set already holds; without a ready peer it fails
- Real mainnet blocks 1 and 1,687,107 round-trip through every wire conversion
