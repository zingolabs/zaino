# `zaino-noderpc` — usage

A POC Zcash **node** JSON-RPC serve adapter over the `NodeRpcService` inner port.
It is the node-RPC sibling of the light-serve adapter: it reads domain types
through a pinned snapshot and converts **domain ↔ wire in the adapter** (both
directions, because node RPC is input-heavy — hex params in, hex/JSON out). It is
a slice, not the production `zaino-serve`.

## What it serves

`getblockcount`, `getbestblockhash`, `getblockchaininfo`, `getblock`,
`getblockheader`, `getrawtransaction`, `sendrawtransaction`, `getmininginfo`,
`getaddressbalance`, `getaddressdeltas`, `validateaddress`, `z_validateaddress`,
`z_listunifiedreceivers`.

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

- Verbosity 1 (zcashd's default, and the explorer's every-block-page call): the
  header and chain-position keys, `size`, `previousblockhash`/`nextblockhash`,
  and `tx` as a list of transaction-id strings.
- Verbosity 2: the same, with `tx` as the decoded transaction DTOs.
- Verbosity 0 (raw hex) is not served yet; it is refused with a message naming
  the served verbosities.

`size` comes from the decoded-block read (`BlockTransactionViews.size`), which is
why both verbosities compose three reads: the block header, its chain position,
and its transactions-with-resolved-inputs.

## Error mapping

`to_error_object` maps each adapter error to a JSON-RPC code per variant, never a
blanket `to_string()` of a `#[source]` cause. Resolving a transaction's inputs is
a server-side concern throughout: a transport failure (`Unavailable`) and the two
source inconsistencies (`MissingPrevout`, `PrevoutIndexOutOfRange` — the
validator served a spending transaction but not the output it spends) all map to
the internal-error code, not invalid-params. A malformed txid or block id is the
only input the caller is blamed for.

## Not modelled here

This adapter stands up no jsonrpsee server for production use; it exercises the
handler shape against the service mock. Mining/peers/txoutset passthrough beyond
`getmininginfo` is out of scope.
