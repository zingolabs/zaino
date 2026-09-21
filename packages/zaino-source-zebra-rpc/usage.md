# `zaino-source-zebra-rpc` — usage

The JSON-RPC adapter: implements the `zaino-source` ports by calling a
validator over JSON-RPC and parsing the replies into domain types.

```rust
use zaino_source_zebra_rpc::ZebraRpcAdapter;

// Waits for the validator to answer (a validator started alongside Zaino is not
// answering yet), resolves auth, and builds the client.
let adapter = ZebraRpcAdapter::connect("127.0.0.1:8232", cookie_path, None, None)
    .await?
    // Opt-in: a consumer that follows the chain needs `SubscribeChainTip`.
    .with_tip_polling(Duration::from_secs(1))
    .await?;
let block = zaino_source::OneShotGetBlock::get_block(&adapter, height).await?;
```

This adapter is Zaino's only validator source: it implements **every** port
that JSON-RPC can answer. Zaino builds its own indexes from the blocks it
fetches here rather than reading the validator's database.

## The chain tip

`get_chain_tip` reads `getbestblockheightandhash`: one call, so the hash and the
height describe the same tip. Two calls (`getbestblockhash` + `getblockcount`)
can straddle a new block and pair one tip's hash with the next tip's height.

JSON-RPC has no push path, so `SubscribeChainTip` is `None` until
`with_tip_polling` is called. The subscription is a `PolledChainTip` over the
adapter's own client: readings arrive at the poll interval, each stamped with
when it was taken, so a subscriber can tell a quiet chain from a validator that
stopped answering. Seeding takes one live read, which is why it is not part of
construction.

## The two halves

- `parse.rs` — `serde_json::Value` → domain types. This is Zaino's
  **external-input validation**: every field is checked, and a reply that does
  not say what it should is an error rather than a default.
- `adapter.rs` — the port impls, and the error classification below, which is
  the part most likely to be got wrong.

## Error classification: the part that matters

Every method must decide whether a validator's error reply is an *answer*
(`QueryError::Domain`) or a *failure* (`QueryError::Fetch`). Four helpers exist
so the decision is made once per class rather than once per method:

| helper | for | maps |
|---|---|---|
| `absent_or_fetch` | height/hash-keyed reads | `-5`, `-8` → the port's "not found" |
| `invalid_address_or_fetch` | address-keyed reads | `-5` only → "invalid address" |
| `call_parsed_optional` | `gettxout` | not-found → `Ok(None)` |
| `submission_rejection` | `sendrawtransaction` | `-22`, `-25..=-27` → the rejection reason |
| `spent_info_rejection` | `getspentinfo` | `-5` → `NotSpent`, `-32601` → `Unsupported` |

Ten methods deliberately use plain `call_parsed`: whole-node-state queries
(`getblockchaininfo`, `getmininginfo`, …) where the only domain error is
"validator not ready", which is not something the node reports with a code.

### Why reading `-8` as "absent" is safe — and where it is not

`-8` also means "your parameter was malformed", so treating it as absence would
hide a Zaino bug as a silent not-found. It is safe on the methods using
`absent_or_fetch` because **every parameter there is rendered from a domain
type**: a `Height` is always a decimal `u32`, a `BlockHash` always exactly 64
hex characters, and the verbosity arguments are literals in the file. A
malformed parameter cannot arise.

The address-keyed methods are the exception, and take `-5` only. There, `-8`
means the *request envelope* was wrong — a bad range, a missing field — which is
a Zaino bug and must stay a failure.

**If you add a method, state which class it is in and why.** This classification
was missing entirely at one point, and the result was that "no block at that
height" arrived as a transport fault and stalled the sync loop against a healthy
validator.

## Byte order at this boundary

Block hashes and txids are **byte-reversed** on the wire and internal-order in
the domain. `hash_to_display_hex` / `txid_to_display_hex` do the outbound
reversal; `parse.rs` does the inbound one. Tree roots, commitments and nonces
are **not** reversed — `parse.rs` has a test for each direction, and they are
there because getting this wrong produces valid-looking hex naming something
that does not exist.

## Related

- ADR-0008 — the port model and the domain/fetch distinction.
- `zaino-rpc` — the transport underneath; it does no parsing.
