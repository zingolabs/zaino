# Lightwallet serving audit

First-principles audit of zainod's `CompactTxStreamer` serving path, against the
goal: one zainod serves as many concurrent light wallets as possible, up to
saturating a 2.5 Gbit/s NIC (~300 MB/s payload).

Evidence tags used throughout:

- **[code]** read in source at the cited `file:line` (tree as of `cleanup/remove_state_backend`,
  2026-09-28)
- **[measured]** a number measured elsewhere in this repo (cited)
- **[est]** arithmetic or engineering estimate, not measured here; every one is a thing the
  harness in §3.5 should replace with a measurement
- **[assumed]** an external fact not verified locally

No index was available locally, so no number in this document was measured by this audit.

## Status (2026-09-28)

**P0 (items 1–6): implemented.**

- Subscription pool.
- PROXY v1/v2 behind `trusted_proxies`.
- Accept back-off, plus the `RLIMIT_NOFILE` raise and boot check.
- `TCP_NODELAY` and `TCP_NOTSENT_LOWAT` 128 KiB.
- Request body caps.
- `LightdInfo` cache (1 s, single flight).
- Address-list length is bounded by the 64 KiB body cap (≈1.5k addresses), not a count.

**P1: implemented, except as noted.**

- **7, lanes:** point, range and scan.
- **8, scan budget:** `max_address_rows`. `Snapshot::range_at_most` stops the walk. `get_many` stays sequential below 64 keys (from 16 in P2).
- **9, precompute:**
  - tip `BlockID` resolved at publication;
  - resident `GetBlock` inline;
  - default projection stored at apply;
  - per-publication memos for tree states and subtree roots.
- **10, mempool:**
  - snapshot framed once per view, one chunk;
  - tails share a per-epoch arrivals log and wake only on arrivals or tip moves;
  - arrivals are still framed per subscriber: one copy, no bigger than the kernel copy of the same bytes.
- **11, metrics:** fixed method/code label tables, handles resolved once.
- **12, stalls:** owed-frame watchdog per connection plus a 30 s request-body deadline. The per-IP work-stream cap is **not** done: it would tighten the CGNAT problem, and the stall timeout closes the permit-holding hole it targeted.

**§3.5 harness: built, not yet run on mainnet.**

- `ztest::loadtest::load` holds the plan, stages and report. It runs as phase `serve` of
  `live-tests/sync/tests/zaino_index_construction.rs`.
- **Client:** raw bytes, never decoded on the load path.
- **Sessions:** pepper-sync steady, librustzcash restore.
- **Correctness:**
  - every served block is byte-consistent;
  - a sample of every answer kind is held to zebra's JSON-RPC.
  - The reference is checked against librustzcash's parse of real mainnet blocks (all field
    kinds).
- **Regtest proof:** `clientless/tests/lightwallet_load.rs`, the per-pool smoke run.
- Every [est] above stands until that phase reports.

---

## 0. Executive summary

**The byte path is not the problem.** Serving `GetBlockRange` costs roughly 0.5–1 ms of CPU
per MB end to end (projection copy + h2 + kernel TCP) [est]. At 300 MB/s that is well under one
core. NIC, disk (on network storage) or page cache bind long before CPU does.

**What caps a zainod today is the shape of its limits and its per-block request bursts, not
bytes.** Ranked by impact:

1. **Idle `GetMempoolStream`s consume the global stream cap.** Every pepper-sync wallet holds
   one mempool stream permanently (`pepper-sync/src/sync.rs:2579-2656`). Each stream holds an
   admission permit until the next block (`admission.rs:70-75`,
   `zaino-chainview/src/view.rs:195-214`). At ~2,048 connected pepper-sync wallets
   (`max_streams` = 2048), every other RPC from every wallet gets `UNAVAILABLE`.
   **Hard ceiling: ~2k steady-state zingo wallets per zainod.**
2. **`max_connections_per_ip` = 32 caps the whole server behind a TLS proxy.** zainod has no TLS
   stack (`docs/running.md:90`), and the documented public deployment is nginx `grpc_pass`
   (`devops/devlog/2026-07-02-reproducible-grpc-funnel.md`). Every connection then arrives from
   one IP. nginx opens one upstream connection per in-flight request [assumed], so the server
   serves **32 concurrent requests in total**. Mobile carrier CGNAT hits the same cap.
3. **One accept error kills zainod.** `EMFILE`/`ENFILE`/`ENOBUFS` from `accept()` returns
   `Err` from the serve loop (`transport.rs:170-179`). `boot` treats any task exit as fatal
   (`zainod/src/indexer.rs:237`). The config allows `max_connections` = 4096, but nothing raises
   or checks `RLIMIT_NOFILE`. A systemd soft limit of 1024 means ~1000 wallets take the daemon
   down.
4. **Unbounded work shares one FIFO 16-permit queue with cheap reads.**
   - Address queries scan all of history, regardless of the requested range
     (`zaino-index-transparent-address/src/serve.rs:135`).
   - Request bodies are unbounded: the router's `decode_request` collects the whole body and
     bypasses tonic's 4 MiB limit (`router.rs:214-233`).
   - Address lists are unbounded.
   - A few heavy requests therefore stall every `GetLatestBlock`, `GetTreeState` and range
     refill behind them (`limits.rs:65-75`).
5. **`GetLightdInfo` costs one validator RPC per call** (`validator.rs:189-194`), with no
   caching. zingo-mobile calls it every 5 s while the app is open (`zingo-mobile/app/rpc/RPC.ts:441`).
6. **No `TCP_NODELAY`.** tonic's own server enables it by default (`tonic-0.14.5
   transport/server/mod.rs:132`). zaino's own accept loop never sets it (`transport.rs:183-195`).
7. **Identical answers are recomputed per wallet on every block.** When a block lands, every
   steady-state wallet asks for the same tip `BlockID`, the same tip `TreeState`, the same
   subtree roots and the same projected tip blocks, and re-reads the whole mempool. zaino
   rebuilds each of these per request: a blocking-pool hop, a hex encode, a protobuf frame, and
   a memcpy of every mempool transaction per subscriber.
8. **Per-stream heap is bounded by the 1 MiB window, not by the 64 KiB send buffer.** A
   default-pools (projected) range chunk is a heap `Vec` of up to 1 MiB (`project.rs:69`).
   hyper polls the next chunk while the previous one drains (hyper-1.10.1
   `proto/h2/mod.rs:185-217`), so a slow client pins about 2 MiB. At 2,048 streams that is about
   4 GiB [est].

Is **"16 permits + `spawn_blocking` per unary request"** right at 10k req/s?

- **Throughput: yes.** By Little's law, 10k/s × ~20–40 µs holds about 0.3 permits [est].
- **Shape: no.**
  - It is a single FIFO lane shared by µs-scale point reads and unbounded scans.
  - It hops even for pure-RAM answers (`GetLatestBlock` at the tip).
  - It caps CPU parallelism for warm reads at 16, whatever the core count.
- **Fix:** lanes with separate budgets, precomputed answers for the tip-derived methods, and
  work budgets on scans (§3.4, items 7–9).

**Capacity today vs after the plan** (reference box: 16 cores, 64 GB RAM, local NVMe, 2.5 GbE)
[est]:

| Workload | Binding resource today | Wallets today | After P0+P1 |
|---|---|---|---|
| Restore from old birthday (byte-bound) | NIC; disk if on network storage | 30–300 concurrent (client scan speed); **32 behind a proxy** | same, NIC-bound, at <1 core |
| Steady state at tip (request-bound) | `max_streams` held by mempool streams | **~2,000** (pepper-sync) | 20–50k per node (bursts + memory per connection) |
| Mobile reconnect (1 day offline) | admission / per-IP | tens/s | ~20 reconnects/s ≈ NIC |

**Plan, top six:**

1. Separate the subscription cap from the work cap.
2. Proxy-aware per-IP limits.
3. Harden the accept loop and `RLIMIT_NOFILE`.
4. `TCP_NODELAY`.
5. Body and list limits.
6. A `GetLightdInfo` cache.

All six are small, local changes; §3.4 has the rest.

---

## 1. Ground truth: the protocol and how wallets use it

### 1.1 The protocol

Normative source: `packages/zaino-proto/proto/{service,compact_formats}.proto`. The `.proto`
comments and the lightwallet-protocol CHANGELOG are the contract. ZIP-307 (Sapling-only) and
ZIP-314 (empty) do not specify this API (`docs/client-requirements.md`, "The contract is the
proto").

| RPC | Shape | Request | Response |
|---|---|---|---|
| `GetLatestBlock` | unary | `ChainSpec` (empty) | `BlockID{height, hash}` |
| `GetBlock` | unary | `BlockID` (height or hash) | `CompactBlock`, **all pools** incl. transparent |
| `GetBlockRange` | server-stream | `BlockRange{start, end, poolTypes[]}` | `CompactBlock`* |
| `GetBlockRangeNullifiers` | server-stream | `BlockRange` | nullifier-only `CompactBlock`* (deprecated) |
| `GetTreeState` / `GetLatestTreeState` | unary | `BlockID` / `Empty` | `TreeState{network, height, hash, time, saplingTree, orchardTree, ironwoodTree}` (trees = hex of `write_commitment_tree`) |
| `GetSubtreeRoots` | server-stream | `{startIndex, shieldedProtocol, maxEntries}` (0 = all) | `SubtreeRoot{rootHash, completingBlockHash, completingBlockHeight}`* |
| `GetTransaction` | unary | `TxFilter` (hash arm only in zaino) | `RawTransaction{data, height}` |
| `SendTransaction` | unary | `RawTransaction` | `SendResponse{errorCode, errorMessage}` |
| `GetTaddressTxids` / `GetTaddressTransactions` | server-stream | `{address, range}` | `RawTransaction`* (full tx bytes) |
| `GetTaddressBalance` / `…Stream` | unary / client-stream | `AddressList` / `Address`* | `Balance` |
| `GetAddressUtxos` / `…Stream` | unary / server-stream | `{addresses[], startHeight, maxEntries}` | list / stream of `GetAddressUtxosReply` |
| `GetMempoolTx` | server-stream | `{exclude_txid_suffixes, poolTypes}` | `CompactTx`* |
| `GetMempoolStream` | server-stream, **long-lived** | `Empty` | `RawTransaction`*; closes when a new block is mined (`service.proto:289-291`) |
| `GetLightdInfo` | unary | `Empty` | `LightdInfo` |

**`poolTypes`:**

- Empty means every shielded pool and no transparent (`router.rs:111-136`, `Pools::default`,
  `project.rs:39-43`).
- A non-empty list is served exactly.
- `POOL_TYPE_INVALID` or an unknown value is `INVALID_ARGUMENT`.
- Both real clients send `pool_types: vec![]` (pepper-sync `client/fetch.rs:191-214`,
  librustzcash `zcash_client_backend/src/sync.rs:352`).
- So **every wallet range request takes the projection path**, never the zero-copy
  `Pools::ALL` path (`project.rs:65`) [code].

**Element sizes on the wire**, derived from `compact_formats.proto` [code + arithmetic]:

| Element | Bytes in its parent (incl. tag + length) |
|---|---|
| `CompactSaplingOutput` (cmu 32, epk 32, ct 52) | 124 |
| `CompactOrchardAction` (nf, cmx, epk 32 each, ct 52) | 159 |
| `CompactSaplingSpend` (nf) | 36 |
| `CompactTxIn` (prevout txid + index) | ~40 |
| `TxOut` (value + P2PKH script) | ~37 |
| `CompactTx` fixed (index, txid, fee) | ~42 |
| `CompactBlock` fixed (height, hash, prevHash, time, chainMetadata) + gRPC frame | ~105 |

### 1.2 What the clients do

Two sync engines matter. Both are read at source (see also `docs/client-requirements.md`).

- **pepper-sync:** zingolib `dev` @ 0838ea8ce, pepper-sync 0.5.0.
- **librustzcash:** `zcash_client_backend` 0.24.0 @ 7f985ada.

The ECC Android/iOS SDKs (Zashi) are **not available locally**, so their batch sizes and
polling are unverified. Zallet calls Zaino in-process, not over gRPC.

**Connection model**

| | pepper-sync | librustzcash reference `sync.rs` |
|---|---|---|
| Connections per wallet | 1 H2 connection; all calls multiplexed (`zingo-netutils/src/lib.rs:325-357`) | 1 client, strictly sequential (`sync.rs:354-358`) |
| Concurrent block streams | ~1: one loader task, load channel capacity 1 (`scan/task.rs:357-369`) | 1, collected fully before the next call |
| Other long-lived streams | 1 `GetMempoolStream`, resubscribed forever (`sync.rs:2579-2656`) | none |
| Client flow control | hyper client defaults: 2 MiB stream / 5 MiB connection window, 16 KiB max frame (hyper-1.10.1 `proto/h2/client.rs:48-50`) | same (tonic) |
| Timeouts | unary 10 s, heavy 20 s, per stream message 15 s (`zingo-netutils/src/time.rs:183-191`) | none |

**Call sites and frequency**

| RPC | pepper-sync | librustzcash |
|---|---|---|
| `GetLatestBlock` | Every continuous-sync iteration, forced at least every `CHECK_NEW_BLOCKS_INTERVAL` = 10 s (`sync.rs:82,492-500`) | Every loop (`sync.rs:315`) |
| `GetBlockRange` | Once per scan range. A range is shard-sized, up to 2^16 notes of blocks. Stream consumed at scan speed, split into loads of ≤2^13 outputs (`task.rs:104-109,525-551`) | Once per `batch_size` batch (caller-supplied; Zallet 1000) |
| `GetTreeState` | Birthday −1 and tip, every new-block iteration (`state.rs:908-935,1017`) | **1:1 with every `GetBlockRange`**, at `start − 1` (`sync.rs:381`) |
| `GetSubtreeRoots` | 3 pools × 2 passes (an unbounded ask plus a confirming empty pass) = **~6 per new block** (`sync.rs:2400-2404`, `client.rs:229-238`) | 3 pools, **from index 0 every run** (`sync.rs:232-260`) |
| `GetBlock` | Reorg check at tip, seams (`sync.rs:569` etc.) | — |
| `GetTaddressTxids` | Per known t-address plus gap-limit discovery, sequential, first iteration (`sync/transparent.rs:61-150`) | — |
| `GetAddressUtxosStream` | dead code | Per account per iteration, all receivers (`sync.rs:505-527`) |
| `GetTransaction` | Per relevant txid, sequential (`scan/transactions.rs:97-110`) | — (SDK) |
| `GetMempoolStream` | Permanent; reconnect immediately on close (`sync.rs:2579-2656`) | — |
| `GetLightdInfo` | zingo-mobile **every 5 s** while active (`zingo-mobile/app/rpc/RPC.ts:441,362-366`) | never |
| `GetBlockRangeNullifiers` | Only `ScannedWithoutMapping` ranges (`task.rs:377-393`) | — |

### 1.3 Workload model

**Chain size.**

- Measured: nothing yet (`docs/running.md:62`: "compact blocks tens of GiB… not measured on a
  full sync").
- Estimate [assumed inputs, flagged], from §1.1 element sizes and assumed mainnet counts:

| Component | Assumed count | × bytes | Estimate |
|---|---|---|---|
| Sapling outputs (Sandblast-dominated) | 50–150 M | 124 | 6–19 GB |
| Orchard actions | 10–60 M | 159 | 2–10 GB |
| Transparent outputs + inputs | ~190 M each (`persistence-architecture.md` §2) | ~37 + ~40 | ~14 GB |
| Transactions | ~20–30 M | ~45 | ~1 GB |
| Blocks | 3.4 M | ~105 | 0.4 GB |
| **`blocks.dat`** | | | **~25–45 GB** |
| **Default-pools answer, Sapling activation → tip** | | | **~10–30 GB** |

**Step 0 of the plan is to replace this table with a measurement** (§3.5). Transparent is
pruned from every wallet answer, but it is still read from disk and page cache
(read amplification, §2.2).

**Workload W1: restore from an old birthday (pepper-sync)** [est]

- **Order:**
  1. GetLatestBlock
  2. GetTreeState ×2
  3. ~6 GetSubtreeRoots
  4. 2–21 GetTaddressTxids (mobile minimal gap vs desktop)
  5. The chain-tip shard range
  6. Historic shard ranges, ascending
  7. One GetTransaction per found tx
- **Bytes:** 10–30 GB per full restore.
- **Requests:** a few thousand (shards × 3 pools plus tree states).
- **Rate per wallet:** bound by client trial-decryption speed, 1–10 MB/s [assumed].
  - Flow control stalls the stream at that pace.
  - The stream holds 1 admission permit and up to ~2 chunks for hours.

**Workload W2: steady state at the tip, per active wallet-hour** [est; 48 blocks/h]

| Source | Requests/h | Bytes/h |
|---|---|---|
| GetLatestBlock (≥ every 10 s) | ~360 | ~20 KB |
| Per block: GetTreeState ×1–2, GetSubtreeRoots ×6, GetBlockRange (verify, ~10 tip blocks) | ~430 | ~2–5 MB [assumed tip block size] |
| GetMempoolStream resubscribe + snapshot (T txs × ~3–10 KB) | 48 | ~5–10 MB |
| GetLightdInfo (zingo-mobile, app open) | 720 | ~0.2 MB |
| **Total** | **~1,500 (0.4 req/s)** | **~10–15 MB (~3–4 KB/s)** |

**W2 is request-bound.**

- Saturating the NIC with it would take ~80–100k wallets, i.e. ~40k req/s on average.
- The traffic is synchronized: every wallet reacts to the same block within ~1 s.
- So the real shape is a **burst of ~10 requests × N wallets per block**.

**Workload W3: mobile reconnect after 1 day** [est]

- ~1,152 blocks, all inside the 1,000-block non-finalised window or just below it. Tip
  compact-block size is assumed at 5–20 KB.
- Plus ~20 small requests.
- Total: 6–23 MB per reconnect.

**Who dominates**

- **Bytes:** `GetBlockRange` by orders of magnitude (W1), then the mempool snapshot fan-out (W2).
- **Request count:** GetLightdInfo (mobile), GetLatestBlock, GetSubtreeRoots and GetTreeState.
  All four are tip-synchronized and **identical across wallets** (W2).

---

## 2. The audit

### 2.1 Common path: every request

```text
accept ─ ConnectionCaps ─ hyper h2 conn task ─ Measured ─ Admission ─ Router ─ dispatch
```

| Step | Code | Cost / issue |
|---|---|---|
| accept | `transport.rs:164-181` | **No `set_nodelay(true)`** (tonic's server defaults to `true`). **Non-transient errors end the server** (`:178`), and `EMFILE` is one of them. |
| caps | `connections.rs:29-47` | Global `Mutex<HashMap<IpAddr, usize>>` per accept/close: fine. The **per-IP cap of 32 breaks proxied and CGNAT deployments** (§0.2). |
| h2 settings | `transport.rs:124-137` | `max_concurrent_streams(8)`, `max_send_buf_size(64 KiB)`, keepalive 30 s/20 s, rapid-reset 128. Receive windows are left at hyper's 1 MiB (irrelevant: requests are tiny). Send pacing = the **client's** 2 MiB stream window and 16 KiB max frame. |
| `Measured` | `observe.rs:33-43,66-90` | Per request: an `Arc<str>` allocation for the method, one boxed future. Per data chunk: `Messages::feed`, O(messages). At close: **4 metric macro calls, each `method.to_owned()`, plus `format!("{code:?}")`** (`emit.rs:122-127`). Plus `first_message` (`:107`) and 2 gauge updates (`emit.rs:83-91`) on one global atomic each. ≈ 3–6 µs and ~7 allocations per request [est]. |
| `Admission` | `admission.rs:69-90` | One `try_acquire_owned` CAS on a **global** semaphore, plus one boxed future. The **permit lives as long as the body**, including idle mempool streams and flow-control-stalled streams. |
| `Router::call` | `router.rs:403-455,509-551` | Per request: clones the service (an `Arc` bump plus a `watch::Receiver` clone, i.e. a shared refcount RMW), clones `DiskReadPermits`, and allocates the path `String` (`:522`). Negligible below ~100k req/s. |
| decode | `router.rs:214-233,240-272` | `body.collect()` with **no size limit**. Claimed paths bypass tonic's codec and its 4 MiB default decode limit. |
| `Served::pin` | `zaino-sync/src/served.rs:41-43` | Two shared atomics per request: a `watch` read lock and an `ArcSwap::load_full` refcount increment. Cross-core cache-line traffic ~0.1–1 µs [est]. Fine at 10k/s, visible at 100k+/s. |
| response | `router.rs:163-209` | Unary: headers + one `Full` body with `grpc-status` in the headers: one write. Streams: one `Frame` per record for `streamed_response`. |

**Framework overhead** (hyper h2 stream setup, HPACK, tower boxing, one task per stream) is
~15–30 µs of CPU per unary request [est, from typical tonic/hyper unary benchmarks; to be
measured]. Per-request tracing is absent from the serve path [code]. h2's internal `trace!`
call sites are statically disabled under the default filter (`zainod/src/logging.rs:20`).

### 2.2 `GetBlockRange` (the byte path)

**Trace for a default-pools request** [code]:

1. **`range()`** decodes the request, parses pools, clamps to the tip, checks `max_range`
   (131,072), and pins a `ReadView` (`serve.rs:118-155`). No read yet.
2. **`range_response` unfold** (`router.rs:619-675`). For each step:
   - **Below the finalised seam** (`next_touches_disk`, i.e. every file step, hot or cold): take
     a `DiskReadPermit` (FIFO), then `spawn_blocking`, which runs `next_chunk`:
     1. `span_from`: reads offsets, up to 1 MiB (`SPAN_BUDGET`, `serve.rs:19`).
     2. `will_need` = `MADV_WILLNEED` (`lib.rs:203`).
     3. `Pages::bytes` (`pages.rs:306-309`) = a zero-copy mmap slice, CRC-checking each 4 KiB
        page the first time the process touches it (`pages.rs:339-362`).
     4. `project`: allocates a `Vec` of `records.len()` (`project.rs:69`).
        - For every `CompactTx` it allocates **another** `Vec` (`project.rs:105`) and copies
          the kept fields into it.
        - Then it copies that `Vec` into `out` (`project.rs:110`).
        - **Two userland copies of every retained byte, one allocation per transaction.**
   - **Above the seam** (the 1,000-block non-finalised window, i.e. every steady-state and
     reconnect request): inline on the runtime worker, **one record per chunk**. Each record
     is projected again for every request (`serve.rs:186-196`).
3. **hyper `PipeToSendStream`** (hyper-1.10.1 `proto/h2/mod.rs:130-230`) polls a chunk,
   reserves its full length, and `send_data`s the whole chunk.
   - h2 splits it into 16 KiB DATA frames (client max frame).
   - `FramedWrite::flush` writes **one DATA frame per `writev`** and breaks the flush loop
     after each data frame (h2-0.4.15 `codec/framed_write.rs:132-165`), i.e. ~64 syscalls/MiB.
   - Payloads ≥256 B are chained (not copied) into `writev`, so the kernel copy is the only
     copy on this leg.
   - While chunk N drains, the body is polled for chunk N+1. So a stream holds **≤2 chunks**,
     and the 64 KiB `SEND_BUFFER` only gates when the next chunk is requested.
4. **Kernel:** `copy_from_user` into skbs, then TSO/GSO. With a TLS proxy there are 3 more
   copies plus AES-GCM in the proxy.

**Copies per served byte:** 2 userland copies (projection) + 1 kernel copy, or 1 kernel copy
for `Pools::ALL`.

**CPU per MB** [est; `persistence-architecture.md` §5.1 measured 92–263 µs/MiB to copy 1 MiB
off a warm mapping]:

| Stage | µs/MB |
|---|---|
| offsets walk + first-touch CRC (first pass only, crc32fast ~15–30 GB/s) | ~0–60 |
| projection (2 copies + field walk) | 150–300 |
| hyper/h2 frame bookkeeping (64 frames) | 30–60 |
| `writev` + TCP send (64 × ~4–6 µs) | 250–400 |
| minor faults on a fresh mapping after each commit (fault-around 16 pages → 16 faults/MiB) | 15–30 |
| **Total zainod + kernel** | **~0.5–0.9 ms/MB** → 0.15–0.3 cores at 300 MB/s |
| TLS proxy on the same box | +0.3–1 ms/MB |

**Findings**

- **B1. Projection runs for every wallet request and copies twice**
  (`project.rs:96-119`). One copy is enough: size the kept fields in one pass, then write
  key + length + spans straight into `out`. That halves the memcpy and removes the
  per-transaction malloc.
- **B2. Tip ranges re-project the same blocks for every wallet.**
  - The non-finalised records are stored full (`non_finalized.rs:52-57`), so every
    steady-state request projects every block again.
  - Store the default projection next to the full record at `apply` (one extra `Bytes` per
    block, 1,000 blocks). The common request then becomes zero-copy.
- **B3. Tip ranges go out one block per chunk**, so one `writev` per block (a 1,000-block
  catch-up = 1,000 syscalls of ~5 KB). Coalescing records into ≥64 KiB chunks cuts syscalls
  3–10× on that path [est].
- **B4. Heap per slow stream is ~2 × 1 MiB** of projected `Vec`, held for as long as the client
  is slow.
  - Mobile at 1 MB/s drains 1 MiB per second, and pepper-sync deliberately stalls the stream
    while it scans (§1.2).
  - 2,048 streams × 2 MiB ≈ 4 GiB of RSS [est].
  - Fix: cap projected chunks at 128–256 KiB, keeping the 1 MiB `WILLNEED` window for disk.
    Or add a global projected-bytes-in-flight budget.
- **B5. Read amplification.** Projection reads full records (transparent included) from page
  cache and disk.
  - Pre-Sandblast ranges are mostly transparent bytes that get dropped.
  - A separate default-projection file makes the common case zero-copy and shrinks the hot
    page-cache set.
  - It costs roughly +40–60% disk [est].
  - Only worth it if measurement shows disk or page cache binding (§3.3). The on-disk format
    stays at version 1 (no production operators).
- **B6. `Pools::ALL` responses fault on the runtime worker.** Nothing touches the pages on the
  blocking thread after the first CRC pass: `bytes()` skips checked pages, and `project` is a
  refcount bump.
  - A page evicted after its first check is therefore faulted inside `writev`, on a tokio worker.
  - `WILLNEED` mitigates this but does not guarantee it (`persistence-architecture.md` §5.6
    states the intent).
  - Rare today (no client sends all pools). Matters once transparent `poolTypes` lands.
- **B7. Remap per commit.** `publish` maps the whole file again on every commit (`lib.rs:347-355`,
  `pages.rs:261`).
  - New readers start with empty page tables (minor faults).
  - Old mappings `munmap` when their last `Bytes` drops, causing TLB-shootdown IPIs.
  - `remap` itself is cheap (235 µs for 100 files [measured], `persistence-architecture.md` §4.2).
    The cost is the refault.
  - "Map once, grow in place" (a large reserved mapping; reads already gated on the sealed
    length) keeps page tables warm.
  - Medium effort, modest gain; do it only if `perf stat -e minor-faults` says so.

### 2.3 `GetBlock`, `GetLatestBlock`

- **`GetBlock`:** decode, then a permit, then `spawn_blocking` (`router.rs:697-719`), then one
  zero-copy record (`serve.rs:89-91`). Fine. pepper-sync uses it for reorg checks at the tip.
- **`GetLatestBlock`:** a permit plus `spawn_blocking` (`router.rs:681-688`) for
  `latest_id()` (`serve.rs:107-112`).
  - At the tip that is a RAM read of the non-finalised `OrdMap` and a framing walk to the hash.
  - So a ~1 µs answer pays a ~10–30 µs hop plus two context switches [est], and **queues behind
    any heavy read** holding the 16 permits.
  - The answer is a pure function of the published view: precompute the framed `BlockID` once
    per publication and serve it without a hop.

### 2.4 Tree state

- **`GetTreeState`** (`router.rs:893-951`, `zaino-index-tree-state/src/view.rs:138-155`):
  - Reads up to 33 nodes per pool across the non-finalised `OrdMap` and the mmap: 3.02 µs warm
    first touch, 296 µs cold [measured], `persistence-architecture.md` §4.
  - Then `from_frontier` and `write_commitment_tree` per pool (no hashing), and **hex encoding**
    of three ~1 KB trees plus `String` allocations.
  - ≈15–25 µs warm [est], plus the hop.
  - Answers for the tip height (pepper-sync) and `start − 1` (librustzcash) repeat across
    wallets. A cache keyed by `(height)` inside the pinned view (lazily filled `OnceLock`
    map, or an LRU sized to the 1,000-block window) makes repeats free.
  - The 412 MiB working set fits in RAM [measured, §2 of that doc].
- **`GetSubtreeRoots`** (`router.rs:953-996`):
  - Per root: 3 allocations (`frame`, 2 × `to_vec`) and **its own DATA frame**, via
    `streamed_response` (`router.rs:193-209`).
  - librustzcash asks for every root from 0 on every run. pepper-sync asks ~6 times per block.
  - Completed roots are append-only: keep one pre-framed buffer per pool, published with the
    view, and serve `slice(start × REC ..)` as a single chunk. Zero allocations, one write.
- **`GetLatestTreeState`:** same as `GetTreeState` at the tip. Not called by either client.

### 2.5 Transparent address methods

`GetAddressUtxos[Stream]`, `GetTaddressBalance[Stream]`, `GetTaddressTransactions`/`Txids`:

- **Unbounded work per request [code].**
  - `receives(address, 0)` walks every LSM segment for the address's whole history, decodes,
    then sorts (`view.rs:33-49`, `lsm/reader.rs:62-80`).
  - Then `get_many` looks up the spend of every receive (`reader.rs:94-108`).
  - `transactions` scans **all history even for a 100-block range** (`zaino-index-transparent-address/src/serve.rs:125-156`).
    pepper-sync asks `last_tip − 100 .. tip` for every address on every session.
  - An exchange or pool address with ~10^6 receives ≈ 1 s of CPU and ~100 MB of RAM in one
    blocking task [est].
  - Address lists are unbounded (`router.rs:1380-1386,1455-1456`).
- **`get_many` uses the global rayon pool** (`par_iter`, `reader.rs:102`).
  - It is called from a tokio blocking thread, which blocks on a rayon latch.
  - For the common 1–100 keys this costs ~10–30 µs of scheduling for ~1 µs of work [est].
  - It competes with the sync fold's `compute` pool (`zaino-sync/src/offload.rs:17-28`).
  - Serving wants inter-request parallelism, not intra-request. Go sequential below a
    threshold, and never use the global pool on the serve path.
- **`GetTaddressTxids` bytes are fetched lazily, one validator `getrawtransaction` at a time**
  (`router.rs:1290-1304`). Latency = n × RTT. An ordered `buffered(k)` would pipeline it.
  Validator-bound either way.

### 2.6 Chain view: mempool and send

- **`GetMempoolStream`** (`router.rs:1116-1140`, `view.rs:178-290`):
  - **One re-encode per subscriber.** `frame(&RawTransaction{data: entry.raw})` memcpys the
    whole transaction into a fresh `Vec` for every subscriber, and each transaction is its own
    DATA frame. `entry.raw` is already `Bytes` (`zaino-proto/build.rs:80` configures `bytes` for
    `RawTransaction.data`). Frame once per entry and share it (`Bytes::clone`). Send the
    initial snapshot as one coalesced chunk.
  - **The herd every block.** Each block closes every tail (`view.rs:264-268`). Every
    pepper-sync wallet reconnects at once and receives the **whole** mempool snapshot again.
    N = 2,048 wallets × ~150 KB ≈ 300 MB per block [est]: a second of full NIC each block,
    re-copied per subscriber.
  - **Irrelevant wakeups.** Every `ChainAction`, including `Sighted`/`Dropped` that tails
    ignore (`view.rs:263`), wakes every tail. The cost is O(N) per action. A
    tail-relevant-only channel (`Admitted`, `TipAdvanced`, `QuorumLost`) removes this.
  - **Admission.** Each tail holds a work permit for its lifetime (see §0.1). This is the
    single largest capacity bug.
- **`GetMempoolTx`:** for each mempool entry, **every request** decodes the consensus
  transaction and renders a `CompactTx` on the async worker (`router.rs:1040-1064`,
  `zainod/src/chainview.rs:81-87`). Neither client calls it today. When one does, precompute
  the `CompactTx` per entry at admission.
- **`SendTransaction`:** `raw.data.to_vec()` (`router.rs:1096`) is one copy, trivial. The
  relay goes to every validator. Validator-bound.

### 2.7 Validator-backed methods

- **`GetLightdInfo`:**
  - Every call makes a validator `getblockchaininfo` round trip (`validator.rs:189-209`),
    through tonic's generated server (`grpc.rs`).
  - With zingo-mobile polling every 5 s, N active apps → N/5 validator RPC/s.
  - Everything in the answer changes at most once per block. Cache it for ≤1 s, or key it on
    the served tip.
- **`GetTransaction`:** one validator RPC. The JSON hex decode on zaino's side is small next to
  the RPC. Validator-bound by design (`validator.rs` module doc).

### 2.8 Cross-cutting

**`DiskReadPermits` (16) + `spawn_blocking` per unary request**

- **Right properties.**
  - It keeps mmap faults off runtime workers, and cold faults are ms-scale
    (`persistence-architecture.md` §5.1).
  - Range streams take a permit per 1 MiB step, not per request (`router.rs:632-652`).
- **Wrong shape for high concurrency.**
  1. **One FIFO lane for all classes.** A µs `GetLatestBlock` waits behind unbounded address
     scans. With 1% heavy (0.5 s) requests at 10k req/s, heavy work alone needs ~50 permits.
     The queue then grows without bound until `max_streams`, and pepper-sync's 10 s unary
     timeout fires [est].
  2. **Hops for RAM answers.** Tip `BlockID`, tip tree state and tip ranges live in the
     non-finalised tier.
  3. **Caps warm-read CPU parallelism at 16**, whatever the core count (arbei has 72c).
  4. **tokio's blocking pool** is a mutex-guarded queue plus a condvar wake per task. That is
     fine at 10k/s, but at 100k/s bursts it is a global serialization point with thread churn.
- **Recommended shape** (§3.4, item 7):
  - A **point lane** (cheap, bounded work, ~2 × cores permits).
  - A **range lane** (I/O queue depth, 16–64).
  - A **scan lane** (address history: small cap plus a per-request row budget).
  - Tip-derived answers precomputed per publication and served inline.

**Metrics:** ~7 allocations and ~8 registry lookups per request (`emit.rs:105-129`) [code].
Replace them with a static per-method handle table resolved once at startup (the methods are a
fixed set of ~20).

**Contention across cores** (all fine below ~50k req/s [est]):

- One global stream semaphore.
- Two global gauge atomics per request.
- `Served::pin` (watch lock + `ArcSwap` refcount).
- The per-IP table mutex (accept/close only).
- The page-check bitmap: relaxed loads only after first touch (`pages.rs:335-337`).

**Page cache:**

- `blocks.dat` is `Access::Normal` plus a `WILLNEED` per window.
- Tree-state files: 412 MiB, effectively resident.
- LSM filters are `Access::Random` (`lsm/reader.rs:43-48`).
- Concurrent restores from different birthdays sweep the whole file. If RAM is below the hot
  set (~25–45 GB [est]), LRU thrashes and each window becomes a ~1 MiB random read. Local NVMe
  absorbs that (≥2 GB/s at QD 16); cloud block storage (e.g. a 125 MB/s baseline) does not.

**HTTP/2 and TCP**

- **Keep:** 16 KiB frames (client-dictated), hyper defaults for receive windows, keepalive, and
  the rapid-reset limit.
- **Add:**
  - `TCP_NODELAY`.
  - Optionally `TCP_NOTSENT_LOWAT` of ~128 KiB. It bounds unsent kernel bytes per socket, which
    caps kernel memory per connection (autotuned `tcp_wmem` can reach MiBs) and
    head-of-line blocking inside a connection.
- **`max_concurrent_streams` = 8:**
  - Fine for direct wallets (pepper-sync ≈ 3 in flight).
  - Too low for a multiplexing proxy (Envoy/Cloudflare): 8 × per-IP 32 = 256 streams
    server-wide.

**DoS and fairness**

- Unbounded request bodies and address lists.
- Unbounded scan work.
- **No stall timeout.** A client that opens streams and never reads holds permits forever;
  h2 pings are answered by its stack. 32 connections × 8 streams = 256 permits per IP, so
  8 IPs take all 2,048.
- The per-IP cap is too tight for proxies and CGNAT, and too loose for slowloris per IP.
- One accept error ends the process.
- The 250 ms pushback (`admission.rs:23`) plus immediate client retries turn exhaustion into a
  retry storm. pepper-sync retries mempool subscriptions every 3 s, and block streams without
  limit.

**Costs that scale with connections, not bytes**

- Idle mempool streams: admission, O(N) wakeups per action, and O(N × mempool) bytes and
  copies per block.
- Keepalive pings: trivial.
- Per-connection h2 and kernel buffers: ~50–100 KB each, plus socket buffers.
- The per-block synchronized request burst of ~10 × N.
- `GetLightdInfo`: O(N) validator RPCs.

**CPU cost summary** [est; to be replaced by §3.5 measurements]

| Operation | CPU per unit |
|---|---|
| Unary framework overhead (h2, tower, admission, metrics) | 20–35 µs / request |
| `spawn_blocking` + permit | 5–15 µs / request (+10–50 µs latency) |
| `GetLatestBlock` work | ~1 µs |
| `GetTreeState` warm | 15–25 µs (3 × 3.02 µs node reads [measured] + serialise + hex) |
| `GetSubtreeRoots` (all, ~3k roots) | 1.5–3 ms (per-root alloc + frame) |
| Address query (typical wallet, ~10–100 rows) | 20–200 µs + rayon ~10–30 µs |
| Range byte path | 0.5–0.9 ms / MB |
| Mempool snapshot per subscriber | ~1 memcpy of the mempool (~150 KB) + 1 frame per tx |

---

## 3. Capacity model and plan

### 3.1 Reference box [assumed]

- 16 cores, 64 GB RAM, local NVMe, 2.5 GbE.
- zainod plus a TLS proxy on the same host.
- Validator elsewhere.
- The cluster nodes' NIC speeds (arbei, tekau) are not documented; check them before using
  either as the reference.

### 3.2 Which resource binds first

| Resource | Ceiling | Binds? |
|---|---|---|
| NIC | ~295 MB/s payload after TCP/TLS/h2 overheads [est] | **Yes, for W1/W3 once the limits are fixed** |
| CPU, byte path | 0.5–0.9 ms/MB + TLS 0.3–1 → ~0.3–0.6 cores at NIC rate | No |
| Syscalls | ~19k `writev`/s at 300 MB/s | No |
| Memory bandwidth | ~4–6 bytes moved per payload byte → ~1.5 GB/s | No |
| Page cache | RAM vs the hot set (25–45 GB `blocks.dat`) | Only if RAM < hot set |
| Disk | NVMe ≥2 GB/s; cloud 125–1000 MB/s; demand = 300 MB/s × read amp (1.2–3×) | **Yes on cloud disks** |
| H2 flow control | 2 MiB/RTT per stream (20 MB/s at 100 ms) | Per-wallet only |
| Blocking pool / permits | 300 hops/s for ranges; ~0.3 permits busy at 10k unary/s | **Only via head-of-line blocking** |
| `max_streams` 2048 | Held by idle mempool streams | **Yes, first: ~2k pepper-sync wallets** |
| `max_connections_per_ip` 32 | Behind a proxy = the whole server | **Yes, first, when proxied** |
| `max_connections` 4096 / fd limit | 1 connection per wallet | **Yes: 4096 wallets, or ~1000 with a 1024 fd soft limit (then the process dies)** |
| Validator RPC | `GetLightdInfo` N/5 per s; `GetTransaction`; t-address fetch | **Yes for mobile-heavy fleets** |
| Unary CPU (after fixes) | ~20–40 µs/request → ~400–800k req/s on 16 cores | At ≥50k wallets, per-block bursts |

### 3.3 Wallets per workload at 2.5 Gbit/s [est]

- **W1, restore:**
  - 300 MB/s ÷ 1–10 MB/s per wallet = **30–300 concurrent restores** saturate the NIC.
  - That is ~50 full restores/hour (≈300 MB/s × 3600 s / 20 GB).
  - Server CPU <1 core.
  - Needs: the proxy-aware per-IP cap, heap bound per stream (B4), and RAM ≥ hot set or local
    NVMe.
- **W2, steady state:**
  - NIC saturation would need ~80–100k wallets.
  - After P0+P1, the binding limits become:
    - per-block bursts: 10 × N requests within ~1–2 s, ≈ 1 core-second per 3–5k wallets with
      caches;
    - per-connection memory: h2 + kernel ≈ 50–200 KB each, so 50k connections ≈ 2.5–10 GB;
    - `RLIMIT_NOFILE`.
  - Realistic target: **20–50k wallets per zainod**.
  - Today: **~2k** (mempool streams hold `max_streams`).
- **W3, reconnect:** 300 MB/s ÷ ~15 MB ≈ **20 reconnects/s** (~70k/hour) at NIC saturation.
- **Mixed:** a fleet is mostly W2 by count and W1 by bytes. A 10k-wallet fleet with 1% restoring
  at any moment ≈ 100 restores (NIC-saturating), plus a 100k-request burst per block.

### 3.4 Prioritized plan

Ordered by impact over effort.

**Measure** (§3.5) means: the loadgen scenario that exercises the item, plus the named
metric. Every item states its expected gain; confirm it before moving to the next tier.

#### P0: capacity bugs (small, local changes; do first)

| # | Change | Files / functions | Gain | Risks | Measure |
|---|---|---|---|---|---|
| 1 | Classify requests at admission. Long-lived subscriptions (`GetMempoolStream`) take a **subscription** permit (new `max_subscriptions`, default = `max_connections`), not a work permit. Optionally bound idle subscriptions per IP. | `admission.rs` (`Admission::call`: path → class), `limits.rs` (`GrpcLimits`), `zainod/src/config.rs` (`GrpcConfig`), `emit.rs` (gauge by class) | Removes the ~2k-wallet ceiling | Subscriptions are then bounded only by connections (memory per tail ≈ a mempool-sized `VecDeque` of `Bytes` refs) | N idle mempool streams + concurrent ranges: zero `UNAVAILABLE` at N > 2048; `active_streams{class}` |
| 2 | Proxy-aware client identity: `trusted_proxies` (CIDRs) + PROXY protocol v2 on accept (a mature crate such as `ppp`/`proxy-header`). Document that nginx needs `proxy_protocol`, or that the per-IP cap must be off behind a proxy. | `transport.rs` (accept loop), `connections.rs` (`admit`), config, `docs/running.md` | Removes the 32-connection server-wide cap behind TLS; restores per-client fairness | PROXY-header parsing on accept (must time out: slowloris); spoofing if the listener is exposed un-proxied (allowlist) | loadgen through nginx: throughput vs connection count |
| 3 | Accept loop: on `EMFILE`/`ENFILE`/`ENOBUFS`/`ENOMEM`, log and back off (e.g. 100 ms → 1 s) and continue (hyper/axum pattern). At boot, raise the soft `RLIMIT_NOFILE` to the hard limit and **refuse a config** whose `max_connections` + index fds + headroom exceeds it. | `transport.rs:170-179`, `zainod/src/main.rs` / `config.rs` (`validate`) | Removes a remote crash; makes the limits real | Crate choice for `setrlimit` (`rlimit` or `libc`; `unsafe` is forbidden in zaino-grpc, so do it in zainod) | fd-exhaustion test: server survives and recovers |
| 4 | `socket.set_nodelay(true)` on every accepted socket. Evaluate `TCP_NOTSENT_LOWAT` ≈ 128 KiB. | `transport.rs:183-195` | Removes Nagle/delayed-ACK stalls (up to ~40 ms) on unary and stream tails | None (tonic's default) | p99 unary latency on a multiplexed connection under mixed load |
| 5 | Bound request bodies (`http_body_util::Limited`: e.g. 64 KiB generally, ~2.1 MiB for `SendTransaction` = max block size + slack) and list lengths (addresses ≤ 1,000 → `INVALID_ARGUMENT`) | `router.rs:214-272` (`decode_request*`), `:1380-1386`, `:1455`, `:1463-1474` | Closes the memory DoS; bounds work per request | Must not reject a legitimate large account (choose limits from client data; lightwalletd has similar caps [assumed]) | adversarial scenario: RSS flat |
| 6 | Cache `LightdInfo`: `ArcSwap<(served tip, Instant, LightdInfo)>`, refreshed at most once per second or on a tip change | `validator.rs:189-209` | Validator RPC load from N/5 per s to ≤1/s | Staleness of `estimated_height` ≤1 s (fine) | validator RPC rate vs mobile-poll scenario |

#### P1: fairness and per-request fixed cost (medium)

| # | Change | Files / functions | Gain | Risks | Measure |
|---|---|---|---|---|---|
| 7 | Lanes instead of one FIFO: `point` (block, tree state, by-hash; ~2 × cores), `range` (window steps; 16–64), `scan` (address history; small, e.g. 4). Each is its own semaphore and wait histogram. | `limits.rs` (`DiskReadPermits` → per-lane), `router.rs` (each dispatch picks its lane) | Heavy scans cannot stall tip reads; warm-read CPU scales with cores | Tuning; more config surface (keep defaults derived from core count) | `disk_read_wait_seconds{lane}` p99 under the mixed scenario with an adversarial hot address |
| 8 | Work budget for address scans: a row cap per request → `RESOURCE_EXHAUSTED` naming the limit. Longer term, a spend-by-address-and-height key so `GetTaddressTransactions` scans only its range. | `zaino-index-transparent-address/src/serve.rs` (`transactions`, `unspent`), `view.rs` | Bounds the worst request from ~seconds to ms | A client with a legitimately huge address gets an error (exchanges, not light wallets). The index change is a design change. | hot-address latency; permit hold time |
| 9 | Precompute per published view (lazy `OnceLock` inside the pinned view, so no invalidation logic): framed tip `BlockID`; a `TreeState` cache for heights in the non-finalised window; a pre-framed subtree-roots buffer per pool (append-only, sliced by `startIndex`); the default-pools projection of each non-finalised record, stored at `apply` (B2). Serve these **inline, no hop**. | `zaino-index-compact-block/src/{serve.rs,non_finalized.rs,view.rs}`, `zaino-index-tree-state/src/{serve.rs,view.rs}`, `router.rs` (`latest`, `tree_state::dispatch`, `subtree_roots`) | Per-block burst CPU for these methods down ~5–10× [est]; tip ranges zero-copy; no hop for tip answers | Memory: +1 projected `Bytes` per window block (~1,000 × tip size); correctness across reorg (the view is immutable, so the cache dies with it) | burst-drain time per block at N wallets; CPU-seconds per burst |
| 10 | Mempool fan-out: frame each entry once (store the framed `RawTransaction` in `MempoolEntry`); send the snapshot as one coalesced chunk; give tails their own action channel (`Admitted`/`TipAdvanced`/`QuorumLost` only) | `zaino-chainview/src/{snapshot.rs,view.rs}` (`tail`, `MempoolTail::next`), `router.rs:1116-1140` | O(N × mempool) memcpy per block → refcount bumps; wakeups only on relevant actions | Channel split must keep the "never miss a tip move" rule (lag → re-anchor) | CPU and bytes per block with N tails |
| 11 | Metrics: a static per-method handle table (`Counter`/`Histogram` resolved once per `(method, code)`); no per-request `String` | `emit.rs`, `observe.rs` | −3–6 µs and ~7 allocations per request [est] | None | `perf` diff; req/s at fixed CPU |
| 12 | Stall and idle policy: reset streams that make no flow-control progress for T minutes; a per-IP cap on concurrent work streams | `admission.rs` (timer on the `Admitted` body), config | Closes permit-holding slowloris | T must exceed pepper-sync's legitimate scan stalls (measure the p99 stall) | adversarial: permits recover |

#### P2: byte-path efficiency and memory (do when measurement shows need)

| # | Change | Files / functions | Gain | Risks | Measure |
|---|---|---|---|---|---|
| 13 | Bound heap per stream: projected chunks ≤128–256 KiB while advising 1 MiB ahead; or a global projected-bytes-in-flight budget | `serve.rs` (`SPAN_BUDGET`, `next_chunk`), `lib.rs` (`span_from`: separate advice and slice sizes) | RSS from ~2 MiB to ~0.5 MiB per slow stream (≈4 GiB → 1 GiB at 2k streams) | 4–8× more hops per MB (~2k/s at NIC rate: fine) | RSS vs N slow streams |
| 14 | Single-copy projection (size pass, then write key + length + spans into `out`; no per-transaction `Vec`) | `project.rs:96-137` | ~2× less memcpy on the hottest path; no per-transaction malloc | Varint-length patching bugs (covered by the existing prost-decode projection tests) | criterion MB/s of `project` |
| 15 | Coalesce small frames: non-finalised records into ≥64 KiB chunks; `streamed_response` concatenates into one `Bytes` | `serve.rs` (`next_chunk` above the seam), `router.rs:193-209` | 3–10× fewer syscalls on tip ranges and roots/UTXO streams [est] | First-message latency (tiny) | syscalls per MB (`perf trace -s`) |
| 16 | `get_many`: sequential below a key threshold; never the global rayon pool on the serve path | `zaino-persistence/src/lsm/reader.rs:94-108` | −10–30 µs per address request; no interference with the sync fold | Large lists lose intra-request parallelism (the lanes handle that) | address-query p50 |
| 17 | Map once, grow in place (no per-commit remap) | `zaino-persistence/src/{pages.rs,fs/real.rs}` | Warm page tables across commits; no `munmap` shootdowns | The SIGBUS invariants of `persistence-architecture.md` §5.2 must hold exactly (reads already gated on the sealed length) | `perf stat -e minor-faults,tlb:tlb_flush` at the tip |
| 18 | Default-projection file (`shielded.dat` + offsets), only if disk or page cache binds | `zaino-index-compact-block` (writer + serve) | Zero-copy for every wallet range; smaller hot set; no read amplification | +40–60% disk [est]; a second artefact to verify (must stay byte-identical to the projection of `blocks.dat`) | page-cache hit %, disk MB/s at a fixed restore mix |

#### P3: validator-bound and rarely called

| # | Change | Files / functions | Gain |
|---|---|---|---|
| 19 | Pipeline `GetTaddressTxids` fetches (ordered `buffered(k)`) | `router.rs:1290-1304` | Latency n × RTT → ~n × RTT / k |
| 20 | Precompute `CompactTx` per mempool entry for `GetMempoolTx` | `zainod/src/chainview.rs`, `router.rs:1040-1064` | Removes per-request transaction parsing (no current client calls it) |
| 21 | `max_concurrent_streams` default 8 → 32 when behind a multiplexing proxy | `limits.rs`, docs | Envoy/Cloudflare-style deployments |

### 3.5 Benchmark and load-test harness

**Step 0: measure the corpus** (replaces §1.3's assumptions). Against a real mainnet index,
produce a histogram per 10k-height band of:

- record size
- default-projected size
- transparent share
- transactions per block
- `blocks.dat`, `offsets.idx` and tree-state totals

A small read-only tool over `CompactBlockStore::reader()` is enough.

**Microbenchmarks** (criterion as a dev-dependency; none exists today):

- `project` MB/s by era band.
- `TreeStateService::treestate` warm and cold (cold dropped from a separate process, per
  `persistence-architecture.md` §1).
- `subtree_roots` full.
- `utxos_of` with 1 / 100 / 10^5 rows.
- `frame()` encodes.
- `Messages::feed`.

**In-process serve benchmark:**

- `GrpcServer` over a real on-disk index (or a synthetic one generated from the Step 0
  histogram) on loopback.
- The client runs in another process, pinned to separate cores.
- Measures req/s and CPU-µs per request per method, CPU-ms per MB per pool set, and the effect
  of each P0–P2 item as an A/B.

**Load generator:** extend `ztest::loadtest`.

- Today it has `LwdClient`, `LoadDriver` and hdrhistogram reports, but only a
  `BlockRangeSweep` scenario, and nothing calls it (`ztest/src/loadtest/`, `scenario.rs:20-22`).
- Session models, one H2 connection per simulated wallet:
  - **`RestoreSession`** (pepper-sync shape):
    - `GetSubtreeRoots` ×6, tree states, shard-sized `GetBlockRange` with `poolTypes = []`.
    - **Consumption paced at a configured client scan rate** (1–10 MB/s). Without pacing,
      flow control, stall and memory effects are invisible.
  - **`SteadySession`:**
    - Triggered by real block events and 10 s ticks: `GetLatestBlock`, 1–2 `GetTreeState`,
      6 `GetSubtreeRoots`, a 10-block verify range, and a `GetMempoolStream` held open and
      re-subscribed on close.
    - Optional 5 s `GetLightdInfo` (mobile).
    - A librustzcash variant: `GetTreeState` per batch, `GetAddressUtxosStream` per iteration.
  - **`ReconnectSession`:** offline for T hours, then catch-up.
  - **`Adversarial`:** unread streams (slowloris), huge bodies, a hot address, a
    connection flood to the fd limit.
  - Add the matching `OpKind` variants.
- **Completeness oracle:** a range returns exactly `end − start + 1` contiguous blocks
  (a known #1378 audit gap).

**Server metrics per run** (Prometheus, already scraped per ztest pod):

- `zaino_grpc_sent_bytes_total`, `requests_total{method,code}`, `first_message_seconds`,
  `duration_seconds`, `disk_read_wait_seconds`, `active_streams`,
  `admission_rejected_total`, `connections_*`.
- Plus cAdvisor/node-exporter: container CPU seconds, RSS, `pgmajfault`, NIC tx bytes.
- **Headline derived numbers:**
  - CPU-seconds per GB served = Δcpu / Δsent_bytes, by scenario.
  - Client p50/p99 per method.
  - Burst-drain time per block.
  - Rejections by class.
  - RSS per stream.
- Guard against a client-bound run: record loadgen CPU and flag a run where a driver exceeds
  ~70% CPU.

**Running it in ztest.** Gaps (from a ztest skim; cite ztest `docs/design-*.md`):

- The runner pod is fixed at 1–2 cores (`src/qos/mod.rs:480-504`).
- There are no load-generator pods.
- There is no node pinning or anti-affinity, only `Pool::{General, Nvme}` (`src/qos/mod.rs:350-353`).
- zainod always builds its index from empty (`src/backends/zainod.rs:984-991`; hours on mainnet).

Cheapest path first:

1. **Piggyback on the sync-tier test.** `live-tests/sync/tests/zaino_index_construction.rs:111-150`
   already builds a mainnet index on NVMe under a 48 h cap. Add a "serve under load" phase
   after the index reaches the tip, and run `LoadDriver` from the driver pod first.
2. **Add a load-client topology component.** N pods × footprint, anti-affinity to zainod so
   traffic crosses the real NIC, with `spec.nodeName` recorded in the report. Aggregate the
   hdrhistograms.
   - **Note:** a single loadgen pod is one source IP. Raise or disable
     `max_connections_per_ip` for the test, or use many pods, or the per-IP cap measures itself.
3. **Add a zaino index snapshot backend** (seed the indexer volume like zebra snapshots), so
   load runs stop paying the index build.
4. **Report.** Extend the segment report reader to load-test windows. Run the never-exercised
   lightwalletd backend through `pair()` for an A/B baseline.

**Acceptance targets** (proposed):

- **Restore mix:** ≥280 MB/s sustained through TLS, zainod CPU <2 cores, p99 first message
  <50 ms, zero `UNAVAILABLE`.
- **Steady:** 20k simulated wallets, per-block burst drained in <2 s, p99 `GetLatestBlock`
  <20 ms, no rejections.
- **Adversarial:** RSS and permit availability recover within the stall timeout.

### 3.6 Assumptions to verify first

- Chain composition and compact sizes (§1.3). Step 0.
- nginx `grpc_pass` opens one upstream connection per in-flight request, with no upstream H2
  multiplexing.
- Client scan speeds of 1–10 MB/s.
- Zashi/ECC SDK batch sizes and polling (not local).
- zingo-mobile pins an older zingolib (2026-03) than the pepper-sync read here (2026-09).
- The cluster NIC speeds, and whether arbei or tekau carries the NVMe pool label.
- Framework per-request cost (20–35 µs) and byte-path cost (0.5–0.9 ms/MB): both estimates, and
  the first thing the in-process benchmark should pin down.
