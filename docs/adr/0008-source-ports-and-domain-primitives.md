# Validator access is a set of single-question ports over domain primitives

## Status

accepted

## Context and decision

`BlockchainSource` was Zaino's abstraction over the backing validator. Its
purpose was right and its boundary was wrong: a 34-method trait whose
signatures returned `zebra_chain`, `zebra_rpc` and `zaino_fetch` types. The
port was declared in the transport's vocabulary rather than Zaino's, and three
consequences followed.

**Errors were unclassifiable.** `BlockchainSourceError` had two variants, both
"Unrecoverable". `zaino-serve` recovered zcashd RPC error codes by
downcast-walking `source()` chains for a `zaino-fetch` connector type, and the
ChainIndex sync loop hand-rolled a retry ladder that counted consecutive
failures, because nothing in the type told retryable from fatal. Worse, the
distinction the code most needed — "the validator answered, and the answer is
*no such block*" versus "the validator could not be reached" — could not be
expressed at all.

**Semantics lived in comments.** `get_block(HashOrHeight)` was documented as
one operation and behaved differently per variant: the hash path fell back to
JSON-RPC for side-chain blocks, the height path did not. `get_best_block_hash`
and `get_best_block_height` were separate calls, so a caller could observe a
torn pair.

**Capability was conflated with preference.** A 3,145-line enum matched in
every method to answer two unrelated questions: *can* this transport serve the
query, and *should* it when both can.

We decide:

1. **Domain types live in `zaino-primitives`**, whose entire dependency list is
   `thiserror`. No serde, no `zebra-*`, no transport vocabulary. This is what
   makes it viable as the crate everything else depends on, and the constraint
   is load-bearing rather than aesthetic — the moment a serde derive lands
   there, the wire format and the domain model start deciding each other.

2. **Each question a consumer can ask is one trait** in `zaino-source`, with
   its own error type. 36 ports, one method each. A method's error enum names
   the answers that method can give: `GetBlockError::HeightNotFound`,
   `SendRawTransactionError::Rejected`, `GetSpentInfoError::NotSpent`.

3. **`QueryError<E>` separates an answer from a failure.** `Domain(E)` is the
   validator replying; `Fetch(FetchError)` is the transport failing.
   `FetchError` carries a machine-readable `FailureMode`
   (`Connection | Timeout | HttpStatus(u16) | RpcError(i64) | Parse | Auth`),
   so retry policy is a function of the type rather than of a comment. The
   `Resilient` decorator retries `Fetch` by `FailureMode` and returns `Domain`
   immediately.

4. **Capability is structural; preference is a routing table.**
   `zaino-source-zebra-readstate` does not implement the mempool or passthrough
   traits, because a read-state service cannot answer those questions. It is
   not possible to route a mempool query to it by mistake. Where both
   transports can answer, `zaino-source-zebra`'s `ZebraValidator` composite
   picks — a short `impl` per trait, not an enum match per method.

5. **Consumer capability aliases live in the consuming crate**, not in
   `zaino-source`. `ChainIndexSourcePorts` is a supertrait of exactly the ports
   ChainIndex uses, declared in `zaino-state`, with a blanket impl. An alias
   states a requirement of its consumer, not a capability of the port crate,
   and `zaino-source` should not have to know who its consumers are.

## The crate graph

```
zaino-primitives   (thiserror only)         domain types
      ↑
zaino-source       (+ tokio)                port traits, errors, Resilient, MockChain
      ↑                    ↑
zaino-rpc                  zaino-convert-zebra   (+ zebra-chain)
(HTTP + JSON-RPC envelope)
      ↑                    ↑
zaino-source-zebra-rpc     zaino-source-zebra-readstate
                  ↑        ↑
              zaino-source-zebra  (ZebraValidator composite)
                       ↑
              zaino-state / zaino-serve
```

`zaino-rpc` is **transport only**: HTTP, the JSON-RPC envelope, auth, and
retry-on-`-1`. `call()` returns a raw `serde_json::Value`; response parsing is
the adapter's job. This split is why the same client serves the production
adapter and the live tests' independent oracle.

## Considered options

- **Widen `BlockchainSource`'s error enum in place** — rejected: it leaves the
  signatures in transport vocabulary, so `zaino-state` keeps depending on
  `zebra-rpc`, `zebra-chain` and `zaino-fetch` at once, and no subsystem can be
  extracted from it.

- **One trait with 34 methods, in domain types** — rejected: every adapter must
  then implement every method, so a read-state adapter has to `unimplemented!()`
  the mempool. Capability becomes a runtime panic instead of a compile error.

- **Swap the bound and follow the compiler** — attempted and abandoned. Rust
  trait bounds propagate up the call graph, so the change cannot be staged: it
  produced **884 errors at once**, and converging them meant rewriting every
  call site, both mocks, and the passthrough layer in a single non-compiling
  window. `BlockchainSource` instead survives as documented scaffolding with a
  single implementation over the new stack, shrinking as each subsystem moves
  onto the real ports.

- **Put the consumer aliases in `zaino-source`** — rejected: it inverts the
  dependency. The port crate would have to enumerate its consumers, and adding
  a consumer would mean editing the crate below it.

## Consequences

- **`BlockchainSource` still exists**, in `zaino-state/src/chain_index/source.rs`,
  documented as temporary scaffolding with a "do not extend" note. Its single
  implementation, `ValidatorSource<V>`, is generic over the ports, so the
  production composite and the test mocks reach ChainIndex through the same
  conversion code — which makes the mock-backed suites coverage of that
  conversion rather than of a parallel implementation of it.

- **The domain/transport error distinction is not optional.** An adapter that
  returns `Fetch` where it should return `Domain` stalls the sync loop against a
  healthy validator: this was a real regression, caught by the live suite, and
  fixed by giving the RPC adapter the classification it had been missing
  entirely. Every new adapter method must decide which of the two it is
  returning.

- **`zaino-fetch` is deleted.** Its transport is `zaino-rpc`, its inbound
  parsing is `zaino-source-zebra-rpc/parse.rs`, its outbound serialization is
  `zaino-serve`'s wire module (ADR-0009), and its legacy protocol parser moved
  to `live-tests/zaino-testutils` as a test-only independent oracle.

- **`--all-features` and `--no-default-features` both need watching.** A port
  trait's mock lives behind a `testing` feature; gate it on
  `any(test, feature = "testing")` or the crate's own tests silently never
  compile.

## Related

- ADR-0009 — the served JSON schema lives in `zaino-serve`.
- ADR-0007 — block persistence is a row-set boundary. The same doctrine, one
  layer down: named conversions at every boundary, no type serving two roles.
