# `zaino-peers` — usage

Zaino's p2p layer (`docs/design/chainview.md` §5, §6, §8): zebra-network's peer set,
embedded. Peers are discovered and unauthenticated, so nothing here votes: every
header, block and transaction handed out is identified by what zebra recomputes
from its bytes, never by what a peer claimed.

```rust,ignore
use zaino_peers::{PeerConfig, PeerNetwork};

let peers = PeerNetwork::start(PeerConfig::new(NetworkType::Main)).await; // seeds dialed + crawled
let headers = peers.find_headers(&locator, None).await?;                 // raw header bytes
let blocks = peers.blocks_by_hash(&hashes).await?;                       // (hash, Option<bytes>)
let listed = peers.mempool_ids().await?;                                 // one peer's mempool
let txs = peers.transactions_by_id(&listed).await?;                      // (id, Option<bytes>)
let mut heard = peers.announcements();                                   // Announced { peer, ids }

let entries = peers.entries(Some(tip));                                  // §6 candidates
let txid = peers.push_isolated(entries[0], &raw).await?;                 // the bytes' own txid
```

## Dependency

zebra-network and zebra-chain come from the `elicbarbieri/zebra` fork (v6.4.2 on the
same librustzcash, orchard and sapling-crypto forks the workspace patches), pinned in
the root `Cargo.toml`. zebra-network is built on tower 0.4, so this crate uses tower
0.4 for its peer set (`tower::buffer::Buffer`); nothing tower-typed is public.

zebra-network's codec parses in `block_in_place`: `PeerNetwork` needs a
multi-thread tokio runtime.

## Starting

`PeerNetwork::start(config)` wraps zebra-network's `init`:

- `listen_addr` defaults to `127.0.0.1:0`: zebra always opens a listener, and Zaino
  serves no peer. The inbound service answers every request with nothing (`Nil`)
- `initial_peers = None` uses the network's DNS seeders; `Some(list)` replaces them
  (regtest keeps loopback addresses only, as zebrad does)
- `peer_target` (16) is zebra's initial outbound target; `request_timeout` (20 s)
  bounds every request and every push
- `cache_dir = None` disables zebra's peer address cache

It returns once the seeds are dialed and crawled once (`init` awaits one `getaddr`:
seconds, up to zebra's crawl timeout for a seed that never answers).

zebra advertises `NODE_NETWORK` on outbound connections, and Zaino answers peers'
requests with nothing. A peer asking Zaino for blocks times its request out; zebra
offers no knob to advertise fewer services.

## Requests

All go through zebra's peer set, which picks the peer (power of two choices over its
ready peers).

| Method | Request | Returns |
|---|---|---|
| `find_headers(known, stop)` | `FindHeaders` | consensus bytes per header, best-chain order |
| `blocks_by_hash(hashes)` | `BlocksByHash` | `(hash, Some(bytes) / None)` in request order |
| `mempool_ids()` | `MempoolTransactionIds` | `PeerTxId`s (txid + v5 auth digest) |
| `transactions_by_id(ids)` | `TransactionsById` | `(id, Some(bytes) / None)` in request order |

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

## Announcements: `peers: x/y`

`announcements()` subscribes to every transaction `inv` a connected peer sends:
`Announced { peer, ids }`, the sender's address attached by zebra (§5's per-peer
sightings). A subscriber that falls 4,096 announcements behind gets
`RecvError::Lagged` and knows it missed some.

## Submission entries and the isolated push (§6)

- `live()`: outbound peers live in the last few minutes (the ones whose `inv` arrives)
- `entries(tip)`: the live ones whose negotiated protocol version zebra accepts at
  `tip` (`Version::min_remote_for_height`). Which one an attempt goes to, and the
  one-netgroup-per-attempt rule, is `zaino-chainview`'s submission job
- `push_isolated(addr, raw)`: parses `raw` (zebra's parse-time checks and point
  rules), opens a fresh `connect_isolated_tcp_direct` connection to `addr` sharing no
  state with the peer set, sends `PushTransaction`, and closes it. It returns the
  bytes' own txid. A peer answers a push with nothing, so this is delivery, not a
  verdict. Errors: `Wire` (never connected), `Connect`, `Push`, `Timeout`

## Tests

- Loopback, no DNS: a scripted zebra-network regtest node (a full `init`, since
  isolated connections advertise no services and the peer set refuses those as
  outbound peers) serves real mainnet blocks 1–3 and three real transactions, lying
  for one of them. Headers and blocks arrive byte-identical, the lie never labels the
  asked id, an `inv` arrives attributed to the sender, and a push lands byte-identical
  at exactly the chosen address
- Real mainnet blocks 1 and 1,687,107 round-trip through every wire conversion
