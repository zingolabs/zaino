# `zaino-noderpc` — usage

The Zcash **node** JSON-RPC serve adapter over the `NodeRpcService` inner port —
the node-RPC / explorer deployment's serving surface. It is the node-RPC sibling
of the light-serve adapter: it reads domain types through a pinned snapshot and
converts **domain ↔ wire in the adapter** (both directions, because node RPC is
input-heavy — hex params in, hex/JSON out). `JsonRpcServer` stands up a real
jsonrpsee server over the handler and the runtime supervises it as a `RunLoop`
component (`zainod`'s `node-rpc-passthrough` deployment).

## Hash byte order

Every 32-byte hash the domain holds — block hash, previous/next hash, merkle
root and every txid — is in **internal consensus order**, which zcashd and zebra
display **byte-reversed**. The adapter renders them with one named function per
kind (`block_hash_to_display`, `txid_to_display`, `merkle_root_to_display`) and
decodes the matching params back from display order (`blockhash_from_hex`,
`txid_from_hex`), so a client's hash round-trips through a lookup unchanged. The
nonce, block commitments and the Sapling/Orchard tree roots are **not** reversed:
zcashd/zebra display those in natural order, so they render straight through
`to_hex`. (The light-wallet gRPC path is separate and keeps compact-block hashes
in internal order per the lightwalletd protocol.)

## What it serves

`getblockcount`, `getbestblockhash`, `getblockchaininfo`, `getblock`,
`getblockheader`, `getblockhashes`, `getblockhash`, `getblockdeltas`,
`getspentinfo`, `gettxout`, `getrawtransaction`, `sendrawtransaction`,
`getinfo`, `getmininginfo`, `getpeerinfo`, `getnetworksolps`, `getrawmempool`,
`getmempoolinfo`, `getdifficulty`, `getnetworkinfo`, `ping`, `getaddressbalance`,
`getaddressdeltas`, `getaddresstxids`, `getaddressutxos`, `z_gettreestate`,
`z_getsubtreesbyindex`, `validateaddress`, `z_validateaddress`,
`z_listunifiedreceivers`.

`z_gettreestate` and `z_getsubtreesbyindex` read through `TreestateRead`
(passthrough — Zaino indexes no commitment-tree frontier). `z_gettreestate`
takes a height (decimal string) or a block hash (resolved to a height over the
local header read) and nests each active pool under its key as
`{commitments: {finalRoot, finalState}}`: the block hash and `finalRoot` render
in display order, `finalState` as hex in natural order, and an inactive pool
omits its key. `finalRoot` is absent against the RPC backend, which discards it
on parse (a recorded divergence from zebra). `z_getsubtreesbyindex` takes
`pool, startIndex, (limit?)` and returns `{pool, start_index, subtrees}` with
each root in natural order; a `startIndex` past the end is an empty `subtrees`
list, not an error.

`getblockhash` resolves a height to the block's hash (display order) **locally**
over the chain view's header read; a height beyond the chain is zcashd's
out-of-range error (code `-8`, "Block height out of range"), distinct from the
not-found `-5`. `gettxout` reads the validator's live UTXO set through
`TxOutRead` (passthrough — Zaino mirrors no UTXO set, and it is *not* `SpendRead`):
`txid, n, (includemempool=true)`, rendering `{bestblock, confirmations, value,
valueZat, scriptPubKey, coinbase}` for an unspent output and JSON `null` for a
spent or unknown one (both null in the oracle). zcashd's `version` field is not
rendered.

The transparent-address methods (`getaddressbalance`, `getaddressdeltas`,
`getaddresstxids`, `getaddressutxos`) are served **locally** over the engine's
`AddressRead`, composed through the `queries::address_*` helpers so the range
handling and the empty-when-unserviceable policy live in one place.
`getaddresstxids` takes a `{addresses, start, end}` object (the inclusive height
window optional) and returns the touching txids in display order — the union
across all queried addresses, globally sorted by `(height, txid)` and
de-duplicated, as zcashd orders it (not grouped per address).
`getaddressutxos` takes `{addresses}` and returns each unspent output in zcashd's
insight-explorer shape (`address`, `txid` display order, `outputIndex`, `script`
hex, `satoshis`, `height`); `chainInfo` wrapping is not modelled — the response is
the bare array, matching the oracle.

The node-status methods (`getinfo`, `getmininginfo`, `getpeerinfo`,
`getnetworksolps`, `getdifficulty`, `getnetworkinfo`, `ping`) read through
`NodeStatusRead`, a typed passthrough of the validator's own status — facts about
the node, not the chain, so none is indexed. `getinfo` always carries `build` as
a string: clients pattern-match on it. `getnetworkinfo` renders zebra/zcashd's
shape (`version`, `subversion`, `protocolversion`, `localservices`, `timeoffset`,
`connections`, `networks`, `relayfee`, `localaddresses`, `warnings`); `relayfee`
is the shared ZEC float with no `*Zat` sibling, matching the oracle. `getdifficulty`
relays the difficulty float. `ping` returns JSON `null` on success. A not-ready
or unreachable validator is an RPC error on every one of these, never a defaulted
success a warmer would cache.

The methods are listed at the top; `getnetworkinfo` and `ping` are backed by new
typed source ports (`GetNetworkInfo`, `Ping`), `getdifficulty` by the existing
`GetDifficulty` port — all relayed through `NodeStatusRead`.

`getrawmempool` and `getmempoolinfo` read through `MempoolListing`. `getrawmempool`
has two shapes, chosen by its `verbose` parameter (default false): non-verbose is
an array of txid hex; verbose is an *object* keyed by txid, each value carrying
`size`, `fee` (the ZEC float, from the shared `zatoshis_to_zec` helper), `feeZat`
(the exact zatoshi integer beside it), `height` (the entry height), and `time`
(the entry's unix seconds, omitted when the source reports none — absent, not
null). The verbose form is an object rather than an array because the explorer's
warmer pattern-matches its entries as key/value pairs. `getmempoolinfo` reports
the transaction count (`size`) and total serialized bytes (`bytes`).

## The explorer transaction view

`getrawtransaction <txid> 1` and `getblock <block> 2` return the transaction
shape the NightHawk block explorer reads, defined by
`zaino-design/design/explorer-tx-contract.md` (binding). The DTO is rendered once
in `wire.rs` (`transaction_view_to_wire`) and shared by both methods.

Key points of the shape:

- Every ZEC amount is emitted twice — the ZEC float the explorer sums, and the
  exact zatoshis beside it — from the single `zatoshis_to_zec` /
  `signed_zatoshis_to_zec` helper. Never compute a ZEC float another way.
- A coinbase renders `vin: [{coinbase: <scriptSig hex>, sequence}]`; `coinbase`
  is a **string** (the explorer decodes it), not a flag. Coinbase-ness comes from
  `TransactionDetail.coinbase`, not block position.
- A spend renders `{txid, vout, scriptSig{asm, hex}, sequence, value, valueSat,
  address?}`. `scriptSig` and `sequence` come from the transaction's own input
  (threaded through `TransactionDetail.transparent_inputs`, a sibling of the
  indexing input shape); `value` and `address` come from the **resolved** output
  it spends, `address` absent when that script is non-standard.
- The whole transaction carries `hex`, its raw consensus bytes, plumbed through
  the decoded read (`DecodedTransaction.raw` / `DetailedTransaction.raw` →
  `TransactionView.raw`) rather than refetched.
- Each `vout` carries `scriptPubKey: {asm, hex, reqSigs?, addresses?, type?}`;
  `asm` is `zaino_address::script_to_asm` (best-effort for non-standard scripts),
  and `reqSigs`, `addresses` (a one-element array) and `type`
  (`pubkeyhash`/`scripthash`) appear together only for a standard P2PKH/P2SH
  script, decoded by `zaino_address::transparent_address_from_script` (`reqSigs`
  is then `1`).
- Conditional keys follow the envelope: `versiongroupid`/`expiryheight` when
  overwintered; `valueBalance`/`valueBalanceZat`/`vShieldedSpend`/
  `vShieldedOutput` from version 4. `orchard` is emitted on **every** transaction
  (zebra does too): empty actions and a zero balance when the transaction has no
  bundle, because the explorer dereferences `orchard.valueBalance` strictly on v5.
- `in_active_chain` is emitted when derivable: `true`/`false` for a transaction in
  a best-/side-chain block (from the block's confirmations in `getblock 2`, or the
  transaction's location in `getrawtransaction 1`), and omitted for a mempool
  transaction, which is in no chain.
- The Ironwood pool is deliberately **not** rendered — the explorer has no field
  for it (a recorded divergence).

## `getblock` verbosities

- Verbosity 0 (the explorer's search page, testing whether a string is a block):
  the raw consensus bytes as lowercase hex, from `BlockVerboseRead::raw_block`.
- Verbosity 1 (zcashd's default, and the explorer's every-block-page call): the
  header and chain-position keys, `size`, `previousblockhash`/`nextblockhash`,
  and `tx` as a list of transaction-id strings.
- Verbosity 2: the same, with `tx` as the decoded transaction DTOs.

Both verbose forms carry the chain-state keys zebra emits: `blockcommitments` and
`solution` (natural order) from the block header, `finalsaplingroot` /
`finalorchardroot` and the `trees` sizes / `chainSupply` / `valuePools` from the
verbose read (`BlockVerbose`, which gained the two final roots for this), and
`nTx` derived from the transaction count. The value pools render through the same
`value_pool_to_wire` helper as `getblockchaininfo` (ZEC float beside exact
`*Zat`).
- A verbosity above 2 is refused with a message naming the served range. An
  unknown block is a not-found error at every verbosity.

At verbosity 1 and 2, `size` comes from the decoded-block read
(`BlockTransactionViews.size`), which is why both compose three reads: the block
header, its chain position, and its transactions-with-resolved-inputs. A by-height
request resolves the height to a hash once — from the header read — and issues the
position and transaction reads by that hash, so the three cannot straddle a tip
reorg between them. Verbosity 0 needs only the single raw-block read.

## `getblockhashes`

`getblockhashes high low ( {noOrphans, logicalTimes} )` returns the blocks whose
`nTime` lies in the half-open range `[low, high)`, ascending by time then by hash
— the read behind every NightHawk block list and the block warmer. The parameter
order is zcashd's (`high`, the newer timestamp, first), while the domain read
(`BlockHashRead::block_hashes`) takes `(low, high)`; the two are swapped in the
handler. The options object is optional, and so is each key in it:

- `logicalTimes` false or absent renders a bare array of display-order hash
  strings; `true` renders `[{blockhash, logicalts}]` objects, where `logicalts`
  is the block's `nTime` (the explorer's transaction cache sorts on it).
- `noOrphans` is accepted and ignored — the read is active-chain only, so the
  option is already satisfied.

A `high` below `low` is an empty range, so the result is an empty array, not an
error — matching zcashd, whose timestamp index seeks to `low` and stops at the
first entry not below `high`. A range beyond the tip or before genesis is
likewise empty. The failure modes are server-side only: a chain-view hole
(`BlockHashReadError::MissingHeader`) and a tier-read failure (`TierRead`) both
map to the internal-error code, never invalid-params.

## `getblockdeltas`

`getblockdeltas "blockhash"` returns a block's transparent value movements and the
chain-position header fields zcashd reports alongside them. It is **indexer-only**:
Zebra answers `-32601`, so the shape authority is zcashd's `blockToDeltasJSON`
(`rpc/blockchain.cpp`). Zaino does **not** hold a dedicated delta index — the read
is *composed* in the engine (`BlockDeltasRead`, over `zaino-core`) from the reads
the node-RPC set already serves: the block header (`BlockRead`), its chain position
(`BlockVerboseRead`), its resolved transactions (`TransactionViewRead`), and the
header times its median is taken over — read from the local chain (`HeaderRead`)
where it covers them, and from the passthrough header read (`BlockVerboseRead::block_header_verbose`)
for ancestors above the local tip. It therefore adds no source port, and no
validator round trip beyond what `getblock(_, 2)` does unless the local indexer is
still catching up to the block. The block id is resolved to the block's own hash
once (ruling R50) before the composing reads, so the header and the transactions
cannot straddle a reorg.

The response mirrors zcashd's key set and order: `hash`, `confirmations`, `size`,
`height`, `version`, `merkleroot`, `deltas`, `time`, `mediantime`, `nonce`, `bits`,
`difficulty`, `chainwork`, `previousblockhash`, `nextblockhash`. Each `deltas`
entry is `{txid, index, inputs, outputs}`. An `inputs` entry is a **negative**
`satoshis` (the negation of the spent output's value) at its `vin` `index`,
carrying `prevtxid` (display order) and `prevout`; an `outputs` entry is a positive
`satoshis` at its `vout` `index`. Both carry `address` **only** when the
output/spent-output script is a standard P2PKH/P2SH template
(`zaino_address::transparent_address_from_script`), and omit the key entirely for
any other script — matching zcashd's `IsValidDestination` gate. A coinbase has no
`inputs`, mirroring zcashd's `IsCoinBase()` skip.

`satoshis` are integer zatoshis throughout (no ZEC float). `mediantime` is zcashd's
`GetMedianTimePast`: the median of the block's own time and its ten predecessors'
times (heights `h-10 ..= h`), over the block's ancestry walked by hash — from the
local chain where it covers the height, and from the passthrough header read
(following `previousblockhash`) for ancestors above the local tip, so a block served
during catch-up still reports a median. `chainwork` is absent (not `null`) when the
validator does not track it, as in `getblock`; `previousblockhash` is absent for
genesis and `nextblockhash` at the tip. An unknown block is zcashd's not-found
(`-5`, `Block not found`). A block off the main chain is zcashd's orphan error —
`-5` with the exact message `Block is an orphan` — which the composed read raises
when the passthrough verbose block reports negative confirmations, mirroring
`blockToDeltasJSON`. The other composition failure modes are all server-side
(`BlockDeltasError`: a resolution failure, a chain-view hole at or below the local
tip in the median-time window, or a corrupt amount) and map to the internal-error
code, never invalid-params.

## Wire dialects

jsonrpsee speaks strict JSON-RPC 2.0, but the explorer's `zcashex` client speaks
the zcashd/bitcoind dialect. `JsonRpcServer` installs a `tower` HTTP layer
(`transport/dialect.rs`, via `set_http_middleware`) that bridges the two. It owns
this adapter's dialect handling outright — it takes no `zebra-rpc` dependency,
re-deriving Zebra's classification so the adapter stays validator-agnostic.

A request is classified from its `(jsonrpc, params, id)` shape:

- `"jsonrpc": "1.0"` with params and id — the lightwalletd-style 1.0 dialect, the
  one `zcashex` uses;
- absent `jsonrpc` with params and id — bitcoind;
- `"2.0"` — strict 2.0;
- anything else, **including batch arrays** — unknown, passed through untouched
  for jsonrpsee to handle (jsonrpsee serves 2.0 batches natively).

For the two legacy dialects the request's `jsonrpc` is rewritten to `"2.0"` so
jsonrpsee accepts it, and the response is reshaped back:

- both `result` and `error` keys are **always** present — `"error": null` on
  success, `"result": null` on error — because `zcashex` strict-matches the whole
  envelope and crashes on a missing key;
- `jsonrpc` is `"1.0"` for the lightwalletd dialect and absent for bitcoind; the
  `id` is echoed unchanged;
- error replies carry the HTTP status bitcoind's `JSONErrorReply` assigns:
  method-not-found (-32601) → 404, invalid-request (-32600) → 400, and every
  other error — parse errors, **invalid params (-32602)**, internal errors → 500.
  Success is 200.

A 2.0 response keeps today's exact shape and status (a success with no `error`
key, an error at HTTP 200), so existing 2.0 clients are unaffected. The layer also
forces `content-type` to `application/json` when it is missing or `text/plain`
(jsonrpsee does no content sniffing, and `zcashex` sends `text/plain`), caps the
request body at two `MAX_BLOCK_BYTES` plus slack (bounding the largest request,
`sendrawtransaction` of a large transaction), and accepts but ignores basic auth.

## Error mapping

`to_error_object` maps each adapter error to a JSON-RPC code per variant, never a
blanket `to_string()` of a `#[source]` cause. Resolving a transaction's inputs is
a server-side concern throughout: a transport failure (`Unavailable`), the two
source inconsistencies (`MissingPrevout`, `PrevoutIndexOutOfRange` — the
validator served a spending transaction but not the output it spends), and the
policy refusal `PrevoutFanoutTooLarge` (the request would fan out past the
per-request prevout ceiling) all map to the internal-error code, not
invalid-params — the caller asked for a well-formed, valid object in each case. A
malformed txid or block id is the only input the caller is blamed for.

Two codes match zcashd/zebra rather than the generic classes: an unknown block or
transaction (`NotFound`) is code `-5` (`RPC_INVALID_ADDRESS_OR_KEY`), with Zaino's
own accurate message; and a validator that does not implement a passthrough
address method (`AddressReadError::Unsupported`, e.g. `getaddressdeltas` on a
zebra backend) is method-not-found (`-32601`), the truthful code for the
validator's gap, keyed off the typed failure mode rather than the message.

A mempool read (`MempoolReadError`) is likewise internal in all three cases: it
is a validator passthrough, so a `Transient` transport failure, a `Fatal` one, or
a `NotServiceable` stub is a server-side concern, never bad client input. The
transient case must surface as an error rather than a zero-valued success, so the
explorer's metric warmer does not cache an empty mempool over a transport blip.

Composing `getblockdeltas` (`BlockDeltasError`) is internal in every variant except
the orphan: a transaction-view resolution failure, a block/verbose/header read
failure, a chain-view hole at or below the local tip in the median-time window
(`MissingHeader`), and a corrupt amount (`InputValueOutOfRange`) are all
server-side. The exception is `Orphan`, which maps to zcashd's `-5` with the exact
message `Block is an orphan` (a block off the main chain is a client-visible
condition, not a server fault). An unknown block is the `-5` not-found above (the
composed read answers `Ok(None)`), not a `BlockDeltasError`.

Locating a spend for `getspentinfo` (`SpendReadError`) is internal in every
variant too — the spends index not yet built (`NotServiceable`), a mid-swap read
race (`Transient`), or a backend failure (`Fatal`). An unspent or unknown outpoint
is **not** one of these: it is the `-5` not-found path carrying zcashd's own
message (see `getspentinfo` below).

## `getspentinfo`

`getspentinfo {"txid": ..., "index": n}` returns where the transparent outpoint
`(txid, index)` was spent, as zcashd's `{txid, index, height}`: the spending
transaction's id (display order), the input (vin) index of it that consumed the
outpoint, and the height it was mined at. It is **indexer-only** — Zebra answers
`-32601`, so the shape authority is zcashd's `getspentinfo` (`rpc/misc.cpp`).

Served **locally** over the engine's `SpendRead::spend_info` (`zaino-core`), which
reads the tiers' spends index and composes across the seam, so an output created
in the finalised tier and spent in the volatile window reports the window's
spending height. No source port and no validator round trip. An unspent or unknown
outpoint is zcashd's error, not a `null` and not a method-not-found: code `-5`
(`RPC_INVALID_ADDRESS_OR_KEY`) with the message "Unable to get spent info", which
zcashd's `GetSpentIndex` raises identically for both. This contrasts with
`gettxout` above, the live unspent-output lookup, which answers `null`.

The two methods read from different places and can briefly disagree about a
recently spent output. `gettxout` is a live passthrough to the validator and
reflects a spend immediately, returning `null` the moment the spending
transaction is mined. `getspentinfo` is served from the local spends index, so
until indexing reaches the spending block it still reports that outpoint as
unspent or unknown (`-5`). An output spent very recently can therefore read as
spent in `gettxout` yet unspent/unknown in `getspentinfo` for the duration of
that lag. In steady state the non-finalised head tracks the chain tip, so the
window is small; it widens only during initial sync or catch-up after downtime.

## Not modelled here

`gettxoutsetinfo`, the whole-UTXO-set aggregate, is not served here.
