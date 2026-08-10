# `zaino-source` — usage

The driven ports: one trait per question a consumer can ask about the chain,
declared in domain vocabulary, with per-question errors.

## Asking a question

Each port is a single-method trait. Consumers name exactly what they need:

```rust
use zaino_source::{GetBlock, GetChainTip};

async fn sync_one<V: GetBlock + GetChainTip>(validator: &V) -> Result<(), MyError> {
    let (_hash, tip) = validator.get_chain_tip().await?;
    let block = validator.get_block(tip).await?;
    // ...
}
```

A bound is a statement of dependency, and a short one is a design signal. If a
function needs eleven ports, it is probably doing eleven things.

## The error model, which is the point

```rust
pub enum QueryError<E> {
    Domain(E),          // the validator answered, and this is the answer
    Fetch(FetchError),  // the validator could not be reached, or failed
}
```

This distinction is **load-bearing, not cosmetic**:

- `Domain(E)` is returned to the caller immediately. It is not retried, because
  asking again produces the same answer.
- `Fetch(FetchError)` is retried by [`Resilient`](#resilient) according to its
  `FailureMode`, and escalated by consumers when retries are exhausted.

Getting this backwards has a specific, observed failure mode: an adapter that
reported "no block at that height" as a `Fetch` error stalled the ChainIndex
sync loop against a *healthy* validator, because the sync loop asks that
question on every iteration and the retry ladder treated each answer as an
outage.

**When implementing an adapter method, decide explicitly which one you are
returning.** If the validator replied at all, it is almost certainly `Domain`.

`FetchError` carries a machine-readable kind:

```rust
pub enum FailureMode {
    Connection, Timeout, HttpStatus(u16), RpcError(i64), Parse, Auth,
}
```

`RpcError(i64)` is what lets a zcashd legacy code survive from the validator to
the served response.

## Domain errors name answers, not failures

A port's error enum enumerates what that question can be answered with:

```rust
GetBlockError::HeightNotFound(Height)
SendRawTransactionError::Rejected(String)
GetSpentInfoError::{ NotSpent, Unsupported }
```

Write the variant that says what happened. A generic `NotFound` on every port
throws away the thing the caller needs to act on — and, at the serving
boundary, the thing that decides which zcashd error code the client sees.

## `Resilient`

Wraps an adapter and adds retry with backoff. It does **not** implement the port
traits — it has its own methods returning `SourceError`, so a consumer holding a
`Resilient<V>` cannot accidentally be handed a bare adapter:

```rust
use zaino_source::{Resilient, RetryPolicy};

let source = Resilient::new(adapter, RetryPolicy::default());
let block = source.get_block(height).await?; // -> SourceError<GetBlockError>
```

Retryable: `Connection`, `Timeout`, `HttpStatus(>= 500)`, and exactly two RPC
codes — `-1` (work queue full) and `-28` (in warmup). Both mean the node is up
and busy or starting. Every other code is the validator's considered reply.

## Capability is structural

An adapter implements only the ports it can answer.
`zaino-source-zebra-readstate` does not implement the mempool traits, because a
read-state service has no mempool — so routing a mempool query to it is a
compile error rather than a runtime panic. Do not add a port impl that
`unimplemented!()`s; leave it out and let the type system carry the fact.

## Consumer aliases go in the consumer

A crate that needs many ports declares its own supertrait alias, **in its own
crate**, with a blanket impl:

```rust
// in zaino-state, not here
pub trait ChainIndexSourcePorts: GetBlock + GetChainTip + /* ... */ {}
impl<T> ChainIndexSourcePorts for T where T: GetBlock + GetChainTip + /* ... */ {}
```

An alias states a requirement of its consumer, not a capability of this crate.
`zaino-source` should not have to know who its consumers are.

## Testing: `MockChain`

Behind the `testing` feature (and always on for this crate's own tests):

```rust
use zaino_source::mock::MockChain;

let mock = MockChain::new()
    .with_block(block)
    .fail_next(2, FailureMode::Timeout);   // failure injection
```

If you add a mock module elsewhere, gate it `#[cfg(any(test, feature = "..."))]`
— a bare `#[cfg(feature = ...)]` that nothing in the workspace enables means the
module never compiles and its tests silently never run.

## Related

- ADR-0008 — the split, and why the bound-swap approach was abandoned.
- `zaino-primitives` — the vocabulary these ports speak.
- `zaino-source-zebra` — the composite that routes questions to transports.
