# Grant Coverage Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve 11 more of the 32 RPCs committed in zingolabs/zaino#72 on the
node-RPC adapter, taking coverage from 16/32 to 27/32.

The 11 RPCs fall into two groups:
- **Nine that Zebra also serves:** `getblockhash`, `getdifficulty`, `ping`,
  `getnetworkinfo`, `getaddresstxids`, `getaddressutxos`, `z_gettreestate`,
  `z_getsubtreesbyindex`, `gettxout`.
- **Two that only an indexer can serve:** `getblockdeltas`, `getspentinfo`.

**Architecture:** this follows the existing ports layering.
- Where a read already exists in zaino-service, the work is wire only:
  - the address reads are LOCAL since the address-seam merge;
  - the treestate reads are passthrough.
- The node-status family (`NodeStatusRead`) grows by difficulty, network info
  and ping, over typed source ports.
- `getblockdeltas` is composed in zaino-core from the resolved block view
  (`TransactionViewRead::block_transaction_views`). That view already carries
  every input's spent value and address.
- `getspentinfo` re-adds `SpendRead` to `NodeRpcReads` with `Spend = Local`.
  The finalised store and the chain head both implement `SpendRead` since the
  address-seam merge.

**Spec:**
- The commitment is the #72 comment of 2024-11-19.
- The shape oracle is zebra 6.4.2, with live responses captured in-cluster at
  `.superpowers/sdd/2026-10-02-explorer-noderpc-slice-4/zebra-oracle/grant-slice-raw.txt`.
- For `getblockdeltas` / `getspentinfo`, zebra answers `-32601`, so the shape
  authority is zcashd's source: `rpc/blockchain.cpp` and `rpc/misc.cpp`, fetched
  with `gh api repos/zcash/zcash/contents/src/rpc/<file> -H "Accept: application/vnd.github.raw"`.

## Global Constraints

**Errors**
- Errors are typed with `#[source]`, decided per site, never stringified from a
  cause.
- The existing `read_error!` String types are a known exception; do not add new
  ones.

**Code conventions**
- No `as`. No unwrap/expect in production code. No mod.rs. Minimum visibility.
- Edit Rust ONLY with the editor tools; never sed or python.
- Add dependencies with `cargo add`.
- zaino-state is deprecated: make only minimal, compile-forced edits there.

**Wire rules**
- All 32-byte values on the wire are in zcashd DISPLAY byte order. Use the
  existing named render/parse fns in zaino-noderpc `wire.rs`; never hand-roll a
  reversal.
- Floats come from the private `zatoshis_to_zec` helper, with the exact `*Zat`
  value beside them.
- Golden tests pin the exact key set and real oracle values where zebra serves
  the method.

**Architecture**
- Adapters only adapt.
- zaino-core binds RESILIENT source traits only.
- New source ports follow the `#[resilient_port]` pattern, with impls on:
  - the zebra-rpc adapter;
  - the composite `ZebraValidator` (RPC-only ports delegate to `self.rpc`);
  - the `Arc<V>` forward (`zaino-source/src/arc_forward.rs`);
  - the source MockChain.
- The compile-time assertions must still hold:
  - `assert_node_rpc`;
  - zaino-source-zebra's port-coverage assertion;
  - zaino-runtime's `Serves<NodeRpc>` over `ValidatorClient<Arc<ZebraValidator>>`.
- Every requests passes through the zcashd 1.0 dialect layer. Each new method
  gets one zcashex-shaped round trip.

**Gate (after every commit)**
- `cargo test --all-features` for each of: zaino-primitives, zaino-source,
  zaino-convert-zebra, zaino-source-zebra-rpc, zaino-source-zebra, zaino-core,
  zaino-noderpc, zaino-lightserve, zaino-wallet, zaino-runtime (including
  integration tests), zaino-address, zaino-store, zaino-chain-head-service,
  zaino-indexes, zaino-consensus, zainod.
- `cargo test -p zaino-service --features testing` plus `cargo check -p zaino-service`
  with no features.
- clippy `--no-deps --all-targets --all-features -D warnings` on touched crates.
- fmt.
- Grep ALL of packages/ before changing any trait or struct.

**Process**
- Commit forward, never amend. No attribution trailer. No push, no PR.

## Review Focus

1. **`gettxout` semantics.** zcashd returns `null` for a spent or unknown
   outpoint, and the oracle confirms `null` for both. The `includemempool`
   parameter defaults to true.
2. **`getblockdeltas` input sign.** A transparent input is a NEGATIVE
   `satoshis` delta, at its own index, carrying `prevtxid`/`prevout` in display
   order. Coinbase has no inputs.
3. **`getspentinfo` across the seam.** An outpoint created in the finalised
   tier and spent in the non-finalised tier is reported with the spending
   height. An unspent or unknown outpoint is the zcashd error, not `null`.
   Check zcashd.
4. **`z_getsubtreesbyindex` with `start_index` past the end** returns an empty
   `subtrees` list, exactly as zebra does.
5. **`getaddressutxos` / `getaddresstxids` local results versus zebra on the
   same address and range.** Byte order and ordering must match. Compare
   against the oracle.

---

### Task 1: The cheap nine (batched)

**Files:** zaino-noderpc (`rpc.rs`, `lib.rs`, `wire.rs`, `wire/params.rs`,
`wire/response.rs`, `usage.md`). Where a port or read is missing, also
zaino-source / zaino-source-zebra-rpc / zaino-source-zebra / zaino-service /
zaino-core.

- **`getaddresstxids`, `getaddressutxos`:** wire only, over the local
  `AddressRead::tx_ids` / `unspent_outpoints`. Params mirror zcashd:
  `{addresses, start, end}` and `{addresses}`, plus `chainInfo` if zcashd
  supports it.
- **`z_gettreestate`, `z_getsubtreesbyindex`:** wire only, over
  `TreestateRead` (passthrough). The keys and nesting must equal the oracle.
- **`getblockhash`:** height → hash via the existing block or header read,
  local over the chain view. An out-of-range height returns zcashd's error.
- **`getdifficulty`:** add `difficulty()` to `NodeStatusRead` over the existing
  `GetDifficulty` port. Ruling R1 dropped it earlier as YAGNI; it is now
  committed demand.
- **`getnetworkinfo`, `ping`:** add typed ports plus `NodeStatusRead` methods,
  passthrough. `ping` returns `null` on success.
- **`gettxout`:** a passthrough read over the existing `GetTxOut` port. Find or
  add the service read; it is NOT `SpendRead`. `null` for a spent or unknown
  outpoint.

Tests:
- golden key sets against the oracle for each method;
- error paths;
- one zcashex 1.0 round trip per method.

Split commits by family:
1. address reads;
2. treestate;
3. node status;
4. gettxout + getblockhash.

### Task 2: `getblockdeltas` (composed, indexer-only)

**Files:** zaino-service (the read, or a composition over
`TransactionViewRead`), zaino-core, zaino-noderpc.

- Shape per zcashd `getblockdeltas`:
  - header fields: hash, confirmations, size, height, version, merkleroot,
    time, mediantime, nonce, bits, difficulty, chainwork, previousblockhash,
    nextblockhash;
  - `deltas: [{txid, index, inputs: [{address, satoshis (negative), index, prevtxid, prevout}], outputs: [{address, satoshis, index}]}]`.
- Inputs or outputs whose script is not P2PKH/P2SH: mirror zcashd exactly.
  Check whether zcashd omits them or emits them without an address.
- Composition belongs in zaino-core, not the adapter. The adapter renders.
- Reuse `block_transaction_views` and the header/verbose reads. Add no new
  validator round trips beyond what getblock v2 already does.
- Fields we cannot source: `mediantime` comes from `zaino_consensus::block_time`
  median over the 11 predecessors' header times (HeaderRead); `chainwork` is
  absent if unknown. Document the absences.
- Tests:
  - engine test: a transparent spend gives a negative input and positive
    outputs, at the correct indices;
  - wire golden;
  - a dialect round trip.

### Task 3: `getspentinfo` (local, indexer-only)

**Files:** zaino-service `read_sets.rs` (re-add `SpendRead` to
`NodeRpcReads`), zaino-core `routing.rs` (`NodeRpcRouting::Spend = Local`),
zaino-runtime node-RPC deployment (bound check), zaino-noderpc.

- `TransparentHistory` already builds `TransparentSpendsIndex`. Confirm that
  the tier `SpendRead` impls from address-seam satisfy the Local placement.
- Shape per zcashd `getspentinfo`: `{txid, index, height}` of the SPENDING
  input. Error codes and messages per zcashd for an unknown or unspent
  outpoint.
- Tests:
  - an engine test across the seam (Review Focus 3);
  - wire golden;
  - a dialect round trip.
- Update `NodeRpcReads`' doc, which currently explains why `SpendRead` is
  absent.
