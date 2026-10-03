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
`getblockheader`, `getrawtransaction`, `sendrawtransaction`, `getinfo`,
`getmininginfo`, `getpeerinfo`, `getnetworksolps`, `getrawmempool`,
`getmempoolinfo`, `getaddressbalance`, `getaddressdeltas`, `validateaddress`,
`z_validateaddress`, `z_listunifiedreceivers`.

The four node-status methods (`getinfo`, `getmininginfo`, `getpeerinfo`,
`getnetworksolps`) read through `NodeStatusRead`, a typed passthrough of the
validator's own status. `getinfo` always carries `build` as a string: clients
pattern-match on it.

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
- A spend renders `{txid, vout, value, valueSat, address?}`, with `value` and
  `address` taken from the **resolved** output it spends. `address` is absent
  when the spent script is non-standard.
- Each `vout` carries `scriptPubKey: {hex, addresses?, type?}`; `addresses` (a
  one-element array) and `type` (`pubkeyhash`/`scripthash`) appear together only
  for a standard P2PKH/P2SH script, decoded by
  `zaino_address::transparent_address_from_script`.
- Conditional keys follow the envelope: `versiongroupid`/`expiryheight` when
  overwintered; `valueBalance`/`valueBalanceZat`/`vShieldedSpend`/
  `vShieldedOutput` from version 4; `orchard` from version 5 — emitted even with
  no actions, because the explorer dereferences `orchard.valueBalance` strictly
  on v5.
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

A mempool read (`MempoolReadError`) is likewise internal in all three cases: it
is a validator passthrough, so a `Transient` transport failure, a `Fatal` one, or
a `NotServiceable` stub is a server-side concern, never bad client input. The
transient case must surface as an error rather than a zero-valued success, so the
explorer's metric warmer does not cache an empty mempool over a transport blip.

## Not modelled here

This adapter stands up no jsonrpsee server for production use; it exercises the
handler shape against the service mock. The txoutset methods (`gettxout`,
`gettxoutsetinfo`) and spend status (`getspentinfo`) are not served.
