# Explorer node-RPC, slices 1–2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Take the node-RPC serving adapter from 6 stub methods to 12 real
zcashd-shaped JSON-RPC methods, covering 12 of the 17 the NightHawk block
explorer calls.

**Architecture:** Each method is a handler on `NodeRpc<S: NodeRpcService>` in
`zaino-noderpc`, reading domain types through a pinned snapshot and converting
domain↔wire **in the adapter**. Slice 1 adds only handlers and wire types over
capabilities the engine already implements, so it changes no port. Slice 2
deletes the opaque `NodeQueryRelay` port and replaces it with typed
`NodeStatusRead` + `MempoolListing`, each answered by passthrough to the
validator's already-typed `zaino-source` ports.

**Tech Stack:** Rust 2021, `jsonrpsee` (server macros), `serde`, `tokio`,
`thiserror`. Tests are `#[tokio::test]` against `zaino_service::testing::
MockIndexerService`.

**Spec:** `/home/chona/zingo/zingolabs/zaino-design/design/explorer-grant-completion.md`

## Global Constraints

- Branch `feat/noderpc-explorer`, worktree
  `/home/chona/zingo/zingolabs/zaino/noderpc-explorer`. All paths below are
  relative to that worktree.
- Every crate has `unsafe_code = "forbid"` and `missing_docs = "warn"`. Every
  new public item needs a doc comment.
- No `as` casts for numeric conversion. Use `From`/`Into`/`TryFrom`.
- No `mod.rs`. A module with children is `foo.rs` + `foo/`.
- Wire conversion lives in the adapter (`zaino-noderpc/src/wire*`), never on a
  domain type.
- No wildcard match arms. `clippy::wildcard_enum_match_arm` is denied; opt out
  per-site with `#[expect(..., reason = "...")]`.
- **Errors: typed causes via `#[source]`, decided context by context.** Never
  `format!` a cause that has a type into a message string, never `expect`, never
  swallow. `#[from]` only where the conversion needs no added context.
  - Every error type **this plan introduces** carries its cause as
    `#[source]`, so the chain survives to the wire boundary.
  - `zaino-service` cannot depend on `zaino-source`, so a service-layer error
    holds a source-layer cause as
    `#[source] Box<dyn std::error::Error + Send + Sync + 'static>`.
  - Pick the variant that matches *this* site's meaning. An arithmetic overflow
    in the adapter is not a "read failure"; a validator that is starting is not
    the same as one that is unreachable. Reusing a nearby variant because it is
    nearby is a defect.
  - The existing `read_error!`-generated types (`AddressReadError`,
    `TxReadError`, …) are a stringly scaffold with no `#[source]`. Do not add
    new `format!("...: {cause}")` sites against them; where one is unavoidable,
    say so in the report. Repairing the macro is a separate PR (see ledger
    ruling R11).
- **Coverage: every error path gets a unit test, not just the happy path.** A
  task's tests must cover each arm the task introduces — the miss, the
  transient, the invalid input — and must fail if the implementation is removed.
  A test whose assertion holds for a trivially empty input proves nothing.
- `makers fmt` and `makers clippy` must pass before each commit.
- Verify per-crate (`cargo test -p <crate>`), never `--workspace`: this host's
  binutils cannot link `aws-lc-sys`, so a workspace build fails for reasons
  unrelated to the change.
- Commit after every task. Never amend; always commit forward.

## Review Focus

Five input classes the spec implies and that a naive handler will get wrong.
Each has its test pinned to the task that owns the code.

1. **Validator unreachable mid-request must stay an error, never an empty
   success.** The explorer's warmers do `handle_result({:error, _}) -> :ignore`,
   keeping the previous cache; a wrong `Ok([])` *poisons* the cache for 15 s.
   Transient must map to an RPC error. (Tasks 3, 7, 9)
2. **An address with no history is zero/empty, not an error.** A fresh taddr is
   a valid query with an empty answer. (Tasks 1, 2)
3. **A height range whose start exceeds its end yields an empty list, not a
   failure.** The explorer derives ranges from user-supplied dates. (Task 2)
4. **Well-formed hex of the wrong length is a params error at the boundary.** A
   31- or 33-byte "txid" must never reach a read. (Task 3)
5. **A malformed or wrong-network address is `Invalid`, not an error.** zcashd's
   `validateaddress` answers `{"isvalid": false}`; it does not fail. (Task 4)

---

### Task 1: `getaddressbalance`

Establishes the serde dependency, the wire param/response module split, and the
`MockChain` balance fixture. Later tasks reuse all three.

**Files:**
- Modify: `packages/zaino-noderpc/Cargo.toml`
- Create: `packages/zaino-noderpc/src/wire/params.rs`
- Create: `packages/zaino-noderpc/src/wire/response.rs`
- Modify: `packages/zaino-noderpc/src/wire.rs`
- Modify: `packages/zaino-noderpc/src/lib.rs`
- Modify: `packages/zaino-noderpc/src/rpc.rs`
- Modify: `packages/zaino-noderpc/src/error.rs`
- Modify: `packages/zaino-service/src/testing.rs`

**Interfaces:**
- Consumes: `zaino_service::AddressRead::balance(&TransparentAddress,
  HeightRange) -> Result<AddressBalance, AddressReadError>`;
  `zaino_service::testing::{MockChain, MockIndexerService}`.
- Produces:
  - `MockChain.balances: Vec<(String, AddressBalance)>`
  - `wire::params::AddressesParam { addresses: Vec<String> }`
  - `wire::response::AddressBalanceResponse { balance: u64, received: u128 }`
  - `NodeRpc::get_address_balance(&self, AddressesParam) ->
    Result<AddressBalanceResponse, RpcError>`
  - `RpcError::AddressRead(#[from] AddressReadError)`

- [ ] **Step 1: Add the dependencies**

In `packages/zaino-noderpc/Cargo.toml`, under `[dependencies]`:

```toml
serde = { workspace = true, features = ["derive"] }
```

- [ ] **Step 2: Add the balance fixture to the mock**

In `packages/zaino-service/src/testing.rs`, add the field to `MockChain`:

```rust
#[derive(Clone, Default)]
pub struct MockChain {
    pub tip: Option<BlockRef>,
    pub serviceable: Option<ServiceableRange>,
    pub mempool: Vec<MempoolTx>,
    /// Scripted transparent balances, keyed by address string. An address
    /// absent here has no history, which reads as a zero balance.
    pub balances: Vec<(String, AddressBalance)>,
}
```

Replace the `balance` arm of `impl AddressRead for MockSnapshot`:

```rust
    async fn balance(
        &self,
        addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        Ok(self
            .chain
            .balances
            .iter()
            .find(|(scripted, _)| scripted == addr.as_str())
            .map(|(_, balance)| *balance)
            .unwrap_or(AddressBalance {
                balance: Zatoshis::ZERO,
                received: ZatoshisFlowSum::from_summed(0),
            }))
    }
```

Add `Zatoshis` and `ZatoshisFlowSum` to the `zaino_primitives::types` import
list at the top of the file. If `Zatoshis::ZERO` does not exist, use
`Zatoshis::new(0).expect("zero is a valid amount")`.

- [ ] **Step 3: Write the failing tests**

Append to `packages/zaino-noderpc/src/lib.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn address_balance_renders_the_scripted_balance() {
        use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum};
        let engine = MockIndexerService::new(MockChain {
            balances: vec![(
                "t1abc".to_string(),
                zaino_primitives::types::AddressBalance {
                    balance: Zatoshis::new(5_000).expect("valid amount"),
                    received: ZatoshisFlowSum::from_summed(12_000),
                },
            )],
            ..Default::default()
        });
        let node = NodeRpc::new(engine);
        let got = node
            .get_address_balance(crate::wire::params::AddressesParam {
                addresses: vec!["t1abc".to_string()],
            })
            .await
            .expect("balance");
        assert_eq!(got.balance, 5_000);
        assert_eq!(got.received, 12_000);
    }

    /// Review Focus 2: an address with no history is zero, not an error.
    #[tokio::test]
    async fn an_address_with_no_history_is_zero_not_an_error() {
        let node = NodeRpc::new(engine_with_tip(None));
        let got = node
            .get_address_balance(crate::wire::params::AddressesParam {
                addresses: vec!["t1nohistory".to_string()],
            })
            .await
            .expect("an unknown address is a valid query");
        assert_eq!(got.balance, 0);
        assert_eq!(got.received, 0);
    }

    #[tokio::test]
    async fn address_balance_rejects_an_empty_address_list() {
        let node = NodeRpc::new(engine_with_tip(None));
        assert!(matches!(
            node.get_address_balance(crate::wire::params::AddressesParam {
                addresses: Vec::new()
            })
            .await,
            Err(RpcError::InvalidParams(_))
        ));
    }
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p zaino-noderpc address_balance`
Expected: FAIL — `wire::params` does not exist, `get_address_balance` not found.

- [ ] **Step 5: Create the wire param module**

Create `packages/zaino-noderpc/src/wire/params.rs`:

```rust
//! Request parameters, as the explorer's client sends them.
//!
//! Shapes follow `nighthawk-apps/zcashex`, the client real callers use, rather
//! than the prose in the zcashd RPC docs: the address RPCs take a single object
//! parameter, not a positional list.

use serde::Deserialize;

/// The `{"addresses": [...]}` object the address RPCs take as their one
/// positional parameter.
#[derive(Debug, Clone, Deserialize)]
pub struct AddressesParam {
    /// The transparent addresses to query.
    pub addresses: Vec<String>,
}
```

- [ ] **Step 6: Create the wire response module**

Create `packages/zaino-noderpc/src/wire/response.rs`:

```rust
//! Response bodies, in the field names and encodings zcashd uses.
//!
//! Zatoshi quantities render as integers: `balance` is supply-bounded and fits
//! `u64`, while `received` is a lifetime flow total that is not supply-bounded
//! and so renders from `u128`.

use serde::Serialize;

/// The `getaddressbalance` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressBalanceResponse {
    /// Total currently held, in zatoshis.
    pub balance: u64,
    /// Lifetime gross receipts, in zatoshis.
    pub received: u128,
}
```

- [ ] **Step 7: Declare the submodules**

At the top of `packages/zaino-noderpc/src/wire.rs`, below the module docs:

```rust
pub mod params;
pub mod response;
```

- [ ] **Step 8: Add the error variant**

In `packages/zaino-noderpc/src/error.rs`, extend the import and add the variant:

```rust
use zaino_service::error::{
    AddressReadError, BroadcastRejection, ReadError, SpendReadError, Transient,
};
```

```rust
    /// A transparent-address read failed.
    #[error(transparent)]
    AddressRead(#[from] AddressReadError),
```

- [ ] **Step 9: Map the error to a JSON-RPC object**

In `packages/zaino-noderpc/src/rpc.rs`, add an arm to `to_error_object`'s match.
`Transient` is an internal error so the caller retries and the explorer's warmer
keeps its previous cache; `Fatal` is a params error because the address itself
was unusable:

```rust
        RpcError::AddressRead(AddressReadError::Transient(cause)) => {
            (ErrorCode::InternalError, cause)
        }
        RpcError::AddressRead(AddressReadError::Fatal(cause)) => {
            (ErrorCode::InvalidParams, cause)
        }
        RpcError::AddressRead(e @ AddressReadError::NotServiceable(_)) => {
            (ErrorCode::InternalError, e.to_string())
        }
```

Add `use zaino_service::error::AddressReadError;` to `rpc.rs`. If
`AddressReadError` has variants beyond these three, add an arm per variant —
do not add a wildcard.

- [ ] **Step 10: Write the handler**

In `packages/zaino-noderpc/src/lib.rs`, add to `impl<S: NodeRpcService> NodeRpc<S>`:

```rust
    /// `getaddressbalance`: the transparent balance of the requested addresses,
    /// summed. zcashd accepts a list and returns one total, so a multi-address
    /// request sums rather than returning a per-address breakdown.
    pub async fn get_address_balance(
        &self,
        params: AddressesParam,
    ) -> Result<AddressBalanceResponse, RpcError> {
        if params.addresses.is_empty() {
            return Err(RpcError::InvalidParams(
                "addresses must not be empty".into(),
            ));
        }
        let snapshot = self.engine.snapshot().await?;
        let range = full_range(&snapshot);
        let mut balance: u64 = 0;
        let mut received: u128 = 0;
        for address in params.addresses {
            let read = snapshot
                .balance(&TransparentAddress::new(address), range)
                .await?;
            balance = balance
                .checked_add(read.balance.as_u64())
                .ok_or_else(|| RpcError::Read(ReadError::Fatal(
                    "summed balance overflows u64".into(),
                )))?;
            received = received
                .checked_add(u128::from(read.received))
                .ok_or_else(|| RpcError::Read(ReadError::Fatal(
                    "summed receipts overflow u128".into(),
                )))?;
        }
        Ok(AddressBalanceResponse { balance, received })
    }
```

Add a helper below the `impl` block:

```rust
/// The whole serviceable height range of `snapshot`, for the address RPCs,
/// which take no range of their own.
fn full_range(snapshot: &impl ChainSegment) -> HeightRange {
    snapshot.coverage().unwrap_or(HeightRange {
        start: Height::GENESIS,
        end: Height::GENESIS,
    })
}
```

Extend `lib.rs`'s imports:

```rust
use zaino_primitives::types::{Height, HeightRange, Outpoint, TransparentAddress};
use zaino_service::error::ReadError;
use zaino_service::AddressRead;

use crate::wire::params::AddressesParam;
use crate::wire::response::AddressBalanceResponse;
```

Change `mod wire;` to `pub mod wire;` so tests and the rpc trait can name the
param types.

If `ReadError` has no `Fatal(String)` variant, use the closest fatal variant its
definition offers; check `packages/zaino-service/src/error.rs` first.

- [ ] **Step 11: Run the tests to verify they pass**

Run: `cargo test -p zaino-noderpc address_balance`
Expected: PASS, 3 tests.

- [ ] **Step 12: Add the JSON-RPC method**

In `packages/zaino-noderpc/src/rpc.rs`, add to the `#[rpc(server)] trait NodeRpcApi`:

```rust
    #[method(name = "getaddressbalance")]
    async fn address_balance(
        &self,
        params: AddressesParam,
    ) -> Result<AddressBalanceResponse, ErrorObjectOwned>;
```

and to the impl:

```rust
    async fn address_balance(
        &self,
        params: AddressesParam,
    ) -> Result<AddressBalanceResponse, ErrorObjectOwned> {
        self.get_address_balance(params)
            .await
            .map_err(to_error_object)
    }
```

Add to `rpc.rs`'s imports:

```rust
use crate::wire::params::AddressesParam;
use crate::wire::response::AddressBalanceResponse;
```

- [ ] **Step 13: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-noderpc --all-targets -- -D warnings && cargo test -p zaino-noderpc && cargo test -p zaino-service --features testing`
Expected: all pass.

- [ ] **Step 14: Commit**

```bash
git add packages/zaino-noderpc packages/zaino-service/src/testing.rs
git commit -m "feat(noderpc): serve getaddressbalance

First real zcashd-shaped method on the node-rpc adapter, over the AddressRead
capability the engine already implements. Establishes the wire param/response
module split and the mock's scripted-balance fixture that the remaining address
methods build on.

An address with no history answers zero rather than failing: a fresh taddr is a
valid query with an empty answer, and the explorer caches a success but ignores
an error."
```

---

### Task 1b: Extract the shared query layer

Task 1 and `zaino-lightserve` now answer the same question two different ways,
and lightserve's answer is wrong twice. This extracts the shared domain-side
computation before seven more tasks duplicate it.

The duplication, concretely — `LightServe::get_taddress_balance`
(`packages/zaino-lightserve/src/lib.rs:173-193`) vs `NodeRpc::get_address_balance`:

- lightserve sums with `total.saturating_add(u64::from(balance.balance))`,
  justified in a comment by "a sum of supply-bounded balances stays below the
  money supply, so this never actually saturates". That is a magnitude argument,
  which this codebase rejects — and `Zatoshis::sum_balances` already exists to
  do it checked.
- lightserve hand-builds `HeightRange { start: GENESIS, end: tip.height }` from
  `pinned_tip()`, ignoring `coverage()`. On a partially-synced chain that claims
  a range the snapshot may not be able to serve.

**What is shared and what is not.** The *computation* (checked summing) and the
*question* (what range is serviceable) are shared. The **policy on no coverage
is not**, and must stay in each adapter: an explorer reporting zero for an
unsynced chain is fine, but a wallet concluding "zero balance" from an unsynced
indexer could make it believe funds are gone. So `serviceable_range` returns
`Option` and each adapter decides — node-RPC answers zero, lightserve keeps
returning `NoBlocks`.

**Files:**
- Create: `packages/zaino-service/src/queries.rs`
- Modify: `packages/zaino-service/src/lib.rs`
- Modify: `packages/zaino-service/usage.md`
- Modify: `packages/zaino-noderpc/src/lib.rs`
- Modify: `packages/zaino-lightserve/src/lib.rs`

**Interfaces:**
- Consumes: `ChainSegment::coverage`, `AddressRead::balance`,
  `Zatoshis::sum_balances`, `ZatoshisFlowSum::checked_join`.
- Produces:
  - `zaino_service::queries::serviceable_range<S: ChainSegment>(&S) -> Option<HeightRange>`
  - `zaino_service::queries::total_balance<S: AddressRead>(&S, &[TransparentAddress], HeightRange) -> Result<AddressBalance, AddressReadError>`
  - `NodeRpc::get_address_balance` and `LightServe::get_taddress_balance` both
    call them; `full_range` in `zaino-noderpc` is deleted.

- [ ] **Step 1: Write the failing tests**

Create the test module at the bottom of the new
`packages/zaino-service/src/queries.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{serviceable_range, total_balance};
    use crate::testing::{MockChain, MockIndexerService};
    use crate::TakeSnapshot;
    use zaino_primitives::types::{
        AddressBalance, BlockHash, BlockRef, Height, TransparentAddress, Zatoshis,
        ZatoshisFlowSum,
    };

    fn balance(zats: u64, received: u64) -> AddressBalance {
        AddressBalance {
            balance: Zatoshis::new(zats).expect("valid amount"),
            received: ZatoshisFlowSum::from_summed(received),
        }
    }

    async fn snapshot_with(chain: MockChain) -> impl crate::AddressRead + crate::ChainSegment {
        MockIndexerService::new(chain)
            .snapshot()
            .await
            .expect("snapshot")
    }

    #[tokio::test]
    async fn no_coverage_has_no_serviceable_range() {
        let snapshot = snapshot_with(MockChain::default()).await;
        assert!(serviceable_range(&snapshot).is_none());
    }

    #[tokio::test]
    async fn a_serviceable_range_is_the_snapshots_coverage() {
        let snapshot = snapshot_with(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(10).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            ..Default::default()
        })
        .await;
        let range = serviceable_range(&snapshot).expect("coverage");
        assert_eq!(range.start, Height::GENESIS);
        assert_eq!(u32::from(range.end), 10);
    }

    #[tokio::test]
    async fn total_balance_sums_every_requested_address() {
        let snapshot = snapshot_with(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(10).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            balances: vec![
                ("t1a".to_string(), balance(500, 900)),
                ("t1b".to_string(), balance(250, 400)),
            ],
            ..Default::default()
        })
        .await;
        let range = serviceable_range(&snapshot).expect("coverage");
        let addrs = vec![
            TransparentAddress::new("t1a".to_string()),
            TransparentAddress::new("t1b".to_string()),
        ];
        let total = total_balance(&snapshot, &addrs, range)
            .await
            .expect("balance");
        assert_eq!(total.balance.as_u64(), 750);
        assert_eq!(u128::from(total.received), 1_300);
    }

    /// An empty address list is a well-formed query with a zero answer. Callers
    /// that want to reject it do so at their own wire boundary, where "you sent
    /// no addresses" is a parameter error.
    #[tokio::test]
    async fn no_addresses_totals_zero() {
        let snapshot = snapshot_with(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(10).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            ..Default::default()
        })
        .await;
        let range = serviceable_range(&snapshot).expect("coverage");
        let total = total_balance(&snapshot, &[], range).await.expect("balance");
        assert_eq!(total.balance.as_u64(), 0);
        assert_eq!(u128::from(total.received), 0);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p zaino-service --features testing queries`
Expected: FAIL — module `queries` does not exist.

- [ ] **Step 3: Write the query layer**

Write the module body above the tests in
`packages/zaino-service/src/queries.rs`:

```rust
//! Domain-side query composition, shared by every serving adapter.
//!
//! These are the questions more than one adapter asks of the same capabilities
//! — "what range can this snapshot answer", "what do these addresses hold in
//! total" — answered once, here, so two adapters cannot drift into two
//! different answers.
//!
//! Free functions over the read traits rather than provided trait methods: a
//! port stays a narrow declaration of what a capability *is*, and composition
//! over several reads is a separate concern that does not belong on it.
//!
//! **Policy stays with the caller.** These functions compute; they do not
//! decide what an absent answer means. Whether "nothing is serviceable" should
//! read as zero or as an error depends on who is asking — an explorer showing
//! zero for an unsynced chain is accurate, while a wallet concluding zero
//! balance from an unsynced indexer could report a user's funds as gone. So
//! [`serviceable_range`] returns `Option` and each adapter answers for itself.

use zaino_primitives::types::{
    AddressBalance, HeightRange, TransparentAddress, Zatoshis, ZatoshisFlowSum,
};

use crate::error::AddressReadError;
use crate::reads::AddressRead;
use crate::ChainSegment;

/// The full height range `snapshot` can answer, or `None` when it can answer
/// nothing.
///
/// This is the range to use when a caller supplies none. It is read from
/// [`ChainSegment::coverage`] rather than built from the pinned tip, because a
/// partially-synced snapshot has a tip it cannot serve all the way down to.
pub fn serviceable_range<S: ChainSegment>(snapshot: &S) -> Option<HeightRange> {
    snapshot.coverage()
}

/// The combined transparent balance of `addrs` over `range`.
///
/// Sums with [`Zatoshis::sum_balances`] and [`ZatoshisFlowSum::checked_join`],
/// so an overflow is reported rather than silently clamped. `balance` is
/// supply-bounded and `received` is a lifetime flow total that is not, which is
/// why the two accumulate through different types.
///
/// An empty `addrs` totals zero: a query about no addresses is well-formed and
/// its answer is zero. A caller for whom an empty list is a protocol error
/// rejects it at its own wire boundary.
pub async fn total_balance<S: AddressRead>(
    snapshot: &S,
    addrs: &[TransparentAddress],
    range: HeightRange,
) -> Result<AddressBalance, AddressReadError> {
    let mut balances = Vec::with_capacity(addrs.len());
    let mut received = ZatoshisFlowSum::from_summed(0);
    for addr in addrs {
        let read = snapshot.balance(addr, range).await?;
        balances.push(read.balance);
        received = received.checked_join(read.received).ok_or_else(|| {
            AddressReadError::Fatal("summed lifetime receipts overflow".to_string())
        })?;
    }
    let balance = Zatoshis::sum_balances(balances.into_iter()).ok_or_else(|| {
        AddressReadError::Fatal("summed balance exceeds the money supply".to_string())
    })?;
    Ok(AddressBalance { balance, received })
}
```

- [ ] **Step 4: Export the module**

In `packages/zaino-service/src/lib.rs`, add `pub mod queries;` beside the other
module declarations. Export the module itself, not flattened re-exports — the
`queries::` prefix is what tells a reader at the call site that this is shared
composition rather than a port method.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p zaino-service --features testing queries`
Expected: PASS, 4 tests.

- [ ] **Step 6: Move the node-RPC handler onto it**

In `packages/zaino-noderpc/src/lib.rs`, delete the `full_range` helper and
rewrite the handler body to use the shared functions. Keep the empty-address
parameter check and the no-coverage short-circuit — those are node-RPC's own
wire policy:

```rust
    pub async fn get_address_balance(
        &self,
        params: AddressesParam,
    ) -> Result<AddressBalanceResponse, RpcError> {
        if params.addresses.is_empty() {
            return Err(RpcError::InvalidParams(
                "addresses must not be empty".into(),
            ));
        }
        let snapshot = self.engine.snapshot().await?;
        // Node-RPC policy: nothing serviceable means no indexed history, which
        // an explorer reads correctly as zero.
        let Some(range) = queries::serviceable_range(&snapshot) else {
            return Ok(AddressBalanceResponse {
                balance: 0,
                received: 0,
            });
        };
        let addrs: Vec<TransparentAddress> = params
            .addresses
            .into_iter()
            .map(TransparentAddress::new)
            .collect();
        let total = queries::total_balance(&snapshot, &addrs, range).await?;
        Ok(AddressBalanceResponse {
            balance: total.balance.as_u64(),
            received: u128::from(total.received),
        })
    }
```

Add `use zaino_service::queries;`. Run `cargo test -p zaino-noderpc` — all of
Task 1's tests must still pass unchanged, including
`address_balance_is_zero_when_nothing_is_serviceable`. If any test needs
editing to pass, stop and report: the refactor changed behaviour it should
have preserved.

- [ ] **Step 7: Move the lightserve handler onto it**

In `packages/zaino-lightserve/src/lib.rs`, rewrite
`get_taddress_balance` (currently lines 173-193):

```rust
    pub async fn get_taddress_balance(
        &self,
        addrs: Vec<TransparentAddress>,
    ) -> Result<proto::Balance, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        // Wallet policy, deliberately unlike the explorer's: a wallet must not
        // read "zero" off an indexer that cannot answer, or it reports the
        // user's funds as gone. An unserviceable snapshot is an error here.
        let range = queries::serviceable_range(&snapshot).ok_or(ServeError::NoBlocks)?;
        let total = queries::total_balance(&snapshot, &addrs, range).await?;
        Ok(proto::Balance {
            value_zat: zat_to_i64(total.balance.as_u64()),
        })
    }
```

Add `use zaino_service::queries;`. The proto carries only `value_zat`, so
`received` is dropped here — that is a wire difference, not a lost
computation.

- [ ] **Step 8: Add the lightserve regression test**

The behaviour that changed is which range is queried: `coverage()` instead of
`[GENESIS, pinned_tip]`. Add a test in `packages/zaino-lightserve/src/lib.rs`'s
test module proving the sum is now checked rather than saturating, and that a
snapshot with no coverage still errors:

```rust
    /// A wallet must not be told "zero" by an indexer that cannot answer.
    #[tokio::test]
    async fn taddress_balance_errors_when_nothing_is_serviceable() {
        let serve = LightServe::new(MockIndexerService::new(MockChain::default()));
        assert!(matches!(
            serve.get_taddress_balance(Vec::new()).await,
            Err(ServeError::NoBlocks)
        ));
    }
```

Match the test module's existing construction helpers rather than the names
above if they differ — read the module first.

- [ ] **Step 9: Update the usage guide**

`packages/zaino-service/usage.md` gains a public module. Add a short section
for `queries`, in the voice of the existing sections: what it is (shared
domain-side composition over the ports), the two functions, and the rule that
policy on an absent answer belongs to the caller. This repo's CLAUDE.md
requires the guide to track new public capability.

- [ ] **Step 10: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-service -p zaino-noderpc -p zaino-lightserve --no-deps --all-targets -- -D warnings && cargo test -p zaino-service --features testing && cargo test -p zaino-noderpc && cargo test -p zaino-lightserve`
Expected: all pass.

- [ ] **Step 11: Commit**

```bash
git add packages/zaino-service packages/zaino-noderpc packages/zaino-lightserve
git commit -m "refactor: share the domain-side balance query between both serving adapters

The light-serve and node-rpc adapters answered the same question two different
ways. lightserve summed with saturating_add, justified by a comment arguing that
a sum of supply-bounded balances stays under the money supply -- a magnitude
argument, where Zatoshis::sum_balances already does it checked. It also built
its height range from the pinned tip while ignoring coverage(), so a
partially-synced snapshot was asked for a range it may not serve.

zaino-service::queries now owns the computation: the serviceable range, and a
checked sum across addresses that accumulates balance and lifetime receipts
through their respective types.

Policy stays with each adapter, because it genuinely differs. An explorer
reporting zero for an unsynced chain is accurate; a wallet concluding zero
balance from an indexer that cannot answer would report a user's funds as gone.
So serviceable_range returns Option, node-rpc answers zero, and light-serve
keeps returning NoBlocks."
```

---

### Task 2: `getaddressdeltas`

**Files:**
- Modify: `packages/zaino-noderpc/src/wire/params.rs`
- Modify: `packages/zaino-noderpc/src/wire/response.rs`
- Modify: `packages/zaino-noderpc/src/lib.rs`
- Modify: `packages/zaino-noderpc/src/rpc.rs`
- Modify: `packages/zaino-service/src/testing.rs`

**Interfaces:**
- Consumes: `AddressRead::deltas(&TransparentAddress, HeightRange) ->
  Result<Vec<AddressDelta>, AddressReadError>`; `wire::params::AddressesParam`.
- Produces:
  - `MockChain.deltas: Vec<AddressDelta>`
  - `wire::params::AddressDeltasParam { addresses, start, end, chain_info }`
  - `wire::response::{AddressDeltaEntry, AddressDeltasResponse}`
  - `NodeRpc::get_address_deltas(&self, AddressDeltasParam) ->
    Result<AddressDeltasResponse, RpcError>`

- [ ] **Step 1: Add the deltas fixture to the mock**

In `packages/zaino-service/src/testing.rs`, add to `MockChain`:

```rust
    /// Scripted address deltas, filtered by address and height on read.
    pub deltas: Vec<AddressDelta>,
```

Replace the `deltas` arm of `impl AddressRead for MockSnapshot`:

```rust
    async fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        Ok(self
            .chain
            .deltas
            .iter()
            .filter(|delta| delta.address.as_str() == addr.as_str())
            .filter(|delta| delta.height >= range.start && delta.height <= range.end)
            .cloned()
            .collect())
    }
```

- [ ] **Step 2: Write the failing tests**

Append to `packages/zaino-noderpc/src/lib.rs`'s `mod tests`:

```rust
    fn delta(height: u32, satoshis: i64, addr: &str) -> zaino_primitives::types::AddressDelta {
        use zaino_primitives::types::{SignedZatoshis, TransparentAddress};
        zaino_primitives::types::AddressDelta {
            satoshis: SignedZatoshis::try_new(satoshis).expect("valid delta"),
            txid: TransactionId::from([7u8; 32]),
            index: 0,
            height: Height::try_from(height).expect("valid height"),
            address: TransparentAddress::new(addr.to_string()),
            block_index: Some(1),
        }
    }

    #[tokio::test]
    async fn address_deltas_filters_by_address_and_height() {
        let engine = MockIndexerService::new(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(200).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            deltas: vec![
                delta(100, 5, "t1abc"),
                delta(150, -3, "t1abc"),
                delta(150, 9, "t1other"),
            ],
            ..Default::default()
        });
        let node = NodeRpc::new(engine);
        let got = node
            .get_address_deltas(crate::wire::params::AddressDeltasParam {
                addresses: vec!["t1abc".to_string()],
                start: Some(120),
                end: Some(160),
                chain_info: false,
            })
            .await
            .expect("deltas");
        assert_eq!(got.deltas.len(), 1);
        assert_eq!(got.deltas[0].satoshis, -3);
        assert_eq!(got.deltas[0].height, 150);
        assert!(got.range.is_none(), "chain_info false omits the wrapper");
    }

    #[tokio::test]
    async fn address_deltas_with_chain_info_carries_the_range() {
        let engine = MockIndexerService::new(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(200).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            deltas: vec![delta(100, 5, "t1abc")],
            ..Default::default()
        });
        let node = NodeRpc::new(engine);
        let got = node
            .get_address_deltas(crate::wire::params::AddressDeltasParam {
                addresses: vec!["t1abc".to_string()],
                start: Some(0),
                end: Some(200),
                chain_info: true,
            })
            .await
            .expect("deltas");
        let range = got.range.expect("chain_info true carries the range");
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 200);
    }

    /// `HeightRange` is inclusive, so a range whose start equals its end is a
    /// one-block query that must return that block's delta — not nothing. This
    /// is the boundary a half-open reading gets wrong, and it is silent.
    #[tokio::test]
    async fn a_single_height_range_is_inclusive_not_empty() {
        let engine = MockIndexerService::new(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(200).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            deltas: vec![delta(150, -3, "t1abc")],
            ..Default::default()
        });
        let node = NodeRpc::new(engine);
        let got = node
            .get_address_deltas(crate::wire::params::AddressDeltasParam {
                addresses: vec!["t1abc".to_string()],
                start: Some(150),
                end: Some(150),
                chain_info: false,
            })
            .await
            .expect("deltas");
        assert_eq!(
            got.deltas.len(),
            1,
            "[150, 150] is one block, not an empty range"
        );
    }

    /// A chain with no coverage has no history to report. This must not query a
    /// synthesised range — it must short-circuit, which is why `full_range`
    /// returns `Option`. Scripting a delta that would match proves the
    /// short-circuit actually happens.
    #[tokio::test]
    async fn no_coverage_answers_empty_without_querying() {
        let engine = MockIndexerService::new(MockChain {
            tip: None,
            deltas: vec![delta(0, 5, "t1abc")],
            ..Default::default()
        });
        let node = NodeRpc::new(engine);
        let got = node
            .get_address_deltas(crate::wire::params::AddressDeltasParam {
                addresses: vec!["t1abc".to_string()],
                start: None,
                end: None,
                chain_info: true,
            })
            .await
            .expect("no coverage is a valid query");
        assert!(got.deltas.is_empty(), "nothing is serviceable, so no deltas");
        assert!(got.range.is_none(), "no range to report without coverage");
    }

    /// Review Focus 3: a backwards range is empty, not a failure. The explorer
    /// derives `start`/`end` from user-supplied dates.
    #[tokio::test]
    async fn a_backwards_height_range_is_empty_not_an_error() {
        let engine = MockIndexerService::new(MockChain {
            deltas: vec![delta(100, 5, "t1abc")],
            ..Default::default()
        });
        let node = NodeRpc::new(engine);
        let got = node
            .get_address_deltas(crate::wire::params::AddressDeltasParam {
                addresses: vec!["t1abc".to_string()],
                start: Some(900),
                end: Some(100),
                chain_info: false,
            })
            .await
            .expect("a backwards range is a valid query");
        assert!(got.deltas.is_empty());
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p zaino-noderpc address_deltas backwards_height`
Expected: FAIL — `AddressDeltasParam` and `get_address_deltas` do not exist.

- [ ] **Step 4: Add the param type**

Append to `packages/zaino-noderpc/src/wire/params.rs`:

```rust
/// The `getaddressdeltas` object parameter. `start` and `end` are inclusive
/// block heights, both optional; `chainInfo` asks for the range wrapper around
/// the delta list.
#[derive(Debug, Clone, Deserialize)]
pub struct AddressDeltasParam {
    /// The transparent addresses to query.
    pub addresses: Vec<String>,
    /// First height to include, inclusive.
    #[serde(default)]
    pub start: Option<u32>,
    /// Last height to include, inclusive.
    #[serde(default)]
    pub end: Option<u32>,
    /// Whether to wrap the list with range and chain-tip fields.
    #[serde(default, rename = "chainInfo")]
    pub chain_info: bool,
}
```

- [ ] **Step 5: Add the response types**

Append to `packages/zaino-noderpc/src/wire/response.rs`:

```rust
/// One entry of the `getaddressdeltas` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressDeltaEntry {
    /// Signed change in zatoshis — negative for a spend.
    pub satoshis: i64,
    /// The transaction that caused the delta, as hex.
    pub txid: String,
    /// Input or output index within the transaction.
    pub index: u32,
    /// Position of the transaction within its block, when the source knows it.
    #[serde(rename = "blockindex", skip_serializing_if = "Option::is_none")]
    pub block_index: Option<u32>,
    /// Block height of the delta.
    pub height: u32,
    /// The transparent address affected.
    pub address: String,
}

/// The height range a `chainInfo` request reports alongside its deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeltaRange {
    /// First height included.
    pub start: u32,
    /// Last height included.
    pub end: u32,
}

/// The `getaddressdeltas` response. `range` is present only when the request
/// asked for `chainInfo`, which is how zcashd distinguishes the wrapped form
/// from the bare list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressDeltasResponse {
    /// The deltas, in `(height, blockindex, index)` order.
    pub deltas: Vec<AddressDeltaEntry>,
    /// The queried range, when `chainInfo` was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<DeltaRange>,
}
```

- [ ] **Step 6: Write the handler**

Add to `impl<S: NodeRpcService> NodeRpc<S>` in `lib.rs`:

```rust
    /// `getaddressdeltas`: every balance change touching the requested
    /// addresses over an inclusive height range.
    ///
    /// Both the wire range and [`HeightRange`] are inclusive, so the bounds
    /// pass through unconverted. A range whose start exceeds its end is
    /// answered empty rather than rejected: callers derive these bounds from
    /// user-supplied dates, where an empty day is ordinary. A chain with no
    /// coverage at all answers empty for the same reason — there is no history
    /// to report, which is a result, not a failure.
    pub async fn get_address_deltas(
        &self,
        params: AddressDeltasParam,
    ) -> Result<AddressDeltasResponse, RpcError> {
        if params.addresses.is_empty() {
            return Err(RpcError::InvalidParams(
                "addresses must not be empty".into(),
            ));
        }
        let snapshot = self.engine.snapshot().await?;
        // No coverage means no indexed history, so there are no deltas to
        // report. `full_range` returns `None` rather than a synthesised
        // genesis-only range precisely so this case is distinguishable.
        let Some(coverage) = full_range(&snapshot) else {
            return Ok(AddressDeltasResponse {
                deltas: Vec::new(),
                range: None,
            });
        };
        let start = params
            .start
            .map(Height::try_from)
            .transpose()
            .map_err(|_| RpcError::InvalidParams("start is not a valid height".into()))?
            .unwrap_or(coverage.start);
        let end = params
            .end
            .map(Height::try_from)
            .transpose()
            .map_err(|_| RpcError::InvalidParams("end is not a valid height".into()))?
            .unwrap_or(coverage.end);

        let mut deltas = Vec::new();
        // `HeightRange` is inclusive, so `start == end` is a one-block query
        // and only `start > end` is empty.
        if start <= end {
            let range = HeightRange { start, end };
            for address in &params.addresses {
                let read = snapshot
                    .deltas(&TransparentAddress::new(address.clone()), range)
                    .await?;
                deltas.extend(read.into_iter().map(delta_to_wire));
            }
            deltas.sort_by_key(|entry| (entry.height, entry.block_index, entry.index));
        }

        let range = params.chain_info.then(|| DeltaRange {
            start: start.into(),
            end: end.into(),
        });
        Ok(AddressDeltasResponse { deltas, range })
    }
```

- [ ] **Step 7: Add the domain→wire conversion**

Append to `packages/zaino-noderpc/src/wire.rs`:

```rust
/// Render an address delta for the wire (domain -> wire).
pub(crate) fn delta_to_wire(delta: AddressDelta) -> AddressDeltaEntry {
    AddressDeltaEntry {
        satoshis: delta.satoshis.as_i64(),
        txid: to_hex(delta.txid.into()),
        index: delta.index,
        block_index: delta.block_index,
        height: delta.height.into(),
        address: delta.address.as_str().to_owned(),
    }
}
```

Add to `wire.rs`'s imports:

```rust
use zaino_primitives::types::AddressDelta;

use crate::wire::response::AddressDeltaEntry;
```

Import `delta_to_wire`, `AddressDeltasParam`, `AddressDeltasResponse` and
`DeltaRange` in `lib.rs` alongside the Task 1 imports.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p zaino-noderpc address_deltas backwards_height`
Expected: PASS, 3 tests.

- [ ] **Step 9: Add the JSON-RPC method**

In `rpc.rs`, mirroring Task 1 step 12:

```rust
    #[method(name = "getaddressdeltas")]
    async fn address_deltas(
        &self,
        params: AddressDeltasParam,
    ) -> Result<AddressDeltasResponse, ErrorObjectOwned>;
```

```rust
    async fn address_deltas(
        &self,
        params: AddressDeltasParam,
    ) -> Result<AddressDeltasResponse, ErrorObjectOwned> {
        self.get_address_deltas(params)
            .await
            .map_err(to_error_object)
    }
```

- [ ] **Step 10: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-noderpc --all-targets -- -D warnings && cargo test -p zaino-noderpc && cargo test -p zaino-service --features testing`
Expected: all pass.

- [ ] **Step 11: Commit**

```bash
git add packages/zaino-noderpc packages/zaino-service/src/testing.rs
git commit -m "feat(noderpc): serve getaddressdeltas

The explorer's address page is built from this plus getaddressbalance, and it
passes chainInfo: true, so the response carries the queried range rather than a
bare list.

The wire range is inclusive where the read's HeightRange is half-open. A range
whose start exceeds its end answers empty instead of failing: callers derive the
bounds from user-supplied dates, where an empty day is an ordinary result."
```

---

### Task 3: `getrawtransaction` at verbosity 0

Verbosity 1 is a different capability and lands in slice 4; this task serves the
raw-hex form and rejects any other verbosity explicitly rather than silently
answering the wrong shape.

**Files:**
- Modify: `packages/zaino-noderpc/src/lib.rs`
- Modify: `packages/zaino-noderpc/src/rpc.rs`
- Modify: `packages/zaino-noderpc/src/error.rs`
- Modify: `packages/zaino-noderpc/src/wire.rs`
- Modify: `packages/zaino-service/src/testing.rs`

**Interfaces:**
- Consumes: `RawTransactionRead::raw_transaction(TransactionId) ->
  Result<Option<RawTransaction>, TxReadError>`; `wire::txid_from_hex`.
- Produces:
  - `MockChain.raw_transactions: Vec<(TransactionId, RawTransaction)>`
  - `wire::bytes_to_hex(&[u8]) -> String`
  - `NodeRpc::get_raw_transaction(&self, &str, Option<u32>) ->
    Result<String, RpcError>`
  - `RpcError::TxRead(#[from] TxReadError)`, `RpcError::NotFound(String)`

- [ ] **Step 1: Add the transaction fixture to the mock**

In `packages/zaino-service/src/testing.rs`, add to `MockChain`:

```rust
    /// Scripted raw transactions, keyed by txid.
    pub raw_transactions: Vec<(TransactionId, RawTransaction)>,
```

Replace `impl RawTransactionRead for MockSnapshot`'s body:

```rust
    async fn raw_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        Ok(self
            .chain
            .raw_transactions
            .iter()
            .find(|(scripted, _)| *scripted == id)
            .map(|(_, tx)| tx.clone()))
    }
```

- [ ] **Step 2: Write the failing tests**

Append to `lib.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn raw_transaction_returns_the_scripted_hex() {
        use zaino_primitives::types::{RawTransaction, TransactionLocation};
        let txid = TransactionId::from([0xABu8; 32]);
        let engine = MockIndexerService::new(MockChain {
            raw_transactions: vec![(
                txid,
                RawTransaction {
                    data: vec![0xDE, 0xAD, 0xBE, 0xEF],
                    location: TransactionLocation::BestChain(
                        Height::try_from(42).expect("valid height"),
                    ),
                },
            )],
            ..Default::default()
        });
        let node = NodeRpc::new(engine);
        let got = node
            .get_raw_transaction(&"ab".repeat(32), Some(0))
            .await
            .expect("raw tx");
        assert_eq!(got, "deadbeef");
    }

    #[tokio::test]
    async fn raw_transaction_reports_an_unknown_txid_as_not_found() {
        let node = NodeRpc::new(engine_with_tip(None));
        assert!(matches!(
            node.get_raw_transaction(&"cd".repeat(32), Some(0)).await,
            Err(RpcError::NotFound(_))
        ));
    }

    /// Verbosity 1 needs a capability this slice does not have. Refusing is
    /// honest; answering raw hex to a caller expecting the decoded object is not.
    #[tokio::test]
    async fn raw_transaction_refuses_verbose_until_the_capability_exists() {
        let node = NodeRpc::new(engine_with_tip(None));
        assert!(matches!(
            node.get_raw_transaction(&"ab".repeat(32), Some(1)).await,
            Err(RpcError::InvalidParams(_))
        ));
    }

    /// Review Focus 4: well-formed hex of the wrong length never reaches a read.
    #[tokio::test]
    async fn a_wrong_length_txid_is_rejected_at_the_boundary() {
        let node = NodeRpc::new(engine_with_tip(None));
        for bad in [&"ab".repeat(31), &"ab".repeat(33)] {
            assert!(matches!(
                node.get_raw_transaction(bad, Some(0)).await,
                Err(RpcError::InvalidParams(_))
            ));
        }
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p zaino-noderpc raw_transaction wrong_length_txid`
Expected: FAIL — `get_raw_transaction` does not exist.

- [ ] **Step 4: Add the error variants**

In `error.rs`, extend the import with `TxReadError` and add:

```rust
    /// The requested object is not known to this indexer.
    #[error("{0}")]
    NotFound(String),
    /// A transaction read failed.
    #[error(transparent)]
    TxRead(#[from] TxReadError),
```

- [ ] **Step 5: Add the hex encoder**

Append to `wire.rs`:

```rust
/// Lowercase hex of an arbitrary byte slice (domain -> wire).
pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
```

- [ ] **Step 6: Write the handler**

Add to `impl<S: NodeRpcService> NodeRpc<S>`:

```rust
    /// `getrawtransaction`: the transaction's consensus bytes as hex.
    ///
    /// Only verbosity 0 is served here. The decoded form is a different
    /// capability — `TransactionRead`, which needs a verbose source port — so a
    /// verbose request is refused rather than answered with the raw shape.
    pub async fn get_raw_transaction(
        &self,
        txid_hex: &str,
        verbosity: Option<u32>,
    ) -> Result<String, RpcError> {
        match verbosity.unwrap_or(0) {
            0 => {}
            other => {
                return Err(RpcError::InvalidParams(format!(
                    "verbosity {other} is not served yet; only 0 (raw hex) is available"
                )))
            }
        }
        let txid = txid_from_hex(txid_hex)?;
        let snapshot = self.engine.snapshot().await?;
        let found = snapshot.raw_transaction(txid).await?;
        let tx = found.ok_or_else(|| {
            RpcError::NotFound(format!("no transaction with id {txid_hex}"))
        })?;
        Ok(bytes_to_hex(&tx.data))
    }
```

Add `use zaino_service::RawTransactionRead;` and extend the `crate::wire`
import with `bytes_to_hex`.

- [ ] **Step 7: Map the new errors**

In `rpc.rs`'s `to_error_object`, add:

```rust
        RpcError::NotFound(message) => (ErrorCode::InvalidParams, message),
        RpcError::TxRead(TxReadError::Transient(cause)) => {
            (ErrorCode::InternalError, cause)
        }
        RpcError::TxRead(e) => (ErrorCode::InternalError, e.to_string()),
```

Replace the second arm with one arm per remaining `TxReadError` variant if
clippy's wildcard lint rejects the catch-all; add
`use zaino_service::error::TxReadError;`.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p zaino-noderpc raw_transaction wrong_length_txid`
Expected: PASS, 4 tests.

- [ ] **Step 9: Add the JSON-RPC method**

```rust
    #[method(name = "getrawtransaction")]
    async fn raw_transaction(
        &self,
        txid: String,
        verbosity: Option<u32>,
    ) -> Result<String, ErrorObjectOwned>;
```

```rust
    async fn raw_transaction(
        &self,
        txid: String,
        verbosity: Option<u32>,
    ) -> Result<String, ErrorObjectOwned> {
        self.get_raw_transaction(&txid, verbosity)
            .await
            .map_err(to_error_object)
    }
```

- [ ] **Step 10: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-noderpc --all-targets -- -D warnings && cargo test -p zaino-noderpc && cargo test -p zaino-service --features testing`
Expected: all pass.

- [ ] **Step 11: Commit**

```bash
git add packages/zaino-noderpc packages/zaino-service/src/testing.rs
git commit -m "feat(noderpc): serve getrawtransaction at verbosity 0

Raw consensus bytes as hex, over the RawTransactionRead passthrough the engine
already implements.

Verbosity 1 is refused with an explicit params error rather than silently
answered with the raw shape: the decoded form is TransactionRead, which has no
engine impl and no verbose source port yet. Callers that want it should get a
clear refusal, not hex they will fail to parse."
```

---

### Task 4: `validateaddress` and `z_validateaddress`

Both are pure functions of an address string and a network, already implemented
in `zaino-address`. The adapter needs a network to validate against, so this
task gives `NodeRpc` one.

**Files:**
- Modify: `packages/zaino-noderpc/Cargo.toml`
- Modify: `packages/zaino-noderpc/src/lib.rs`
- Modify: `packages/zaino-noderpc/src/wire/response.rs`
- Modify: `packages/zaino-noderpc/src/wire.rs`
- Modify: `packages/zaino-noderpc/src/rpc.rs`

**Interfaces:**
- Consumes: `zaino_address::{validate_address, z_validate_address,
  ValidatedAddress, ZValidatedAddress}`;
  `zcash_protocol::consensus::Network`.
- Produces:
  - `NodeRpc::new(engine, network)` — **signature change**, two arguments
  - `wire::response::{ValidateAddressResponse, ZValidateAddressResponse}`
  - `NodeRpc::{validate_address, z_validate_address}(&self, &str)`

- [ ] **Step 1: Add the dependencies**

In `packages/zaino-noderpc/Cargo.toml`:

```toml
zaino-address = { path = "../zaino-address" }
zcash_protocol = { workspace = true }
```

- [ ] **Step 2: Write the failing tests**

Append to `lib.rs`'s `mod tests`:

```rust
    /// Review Focus 5: garbage is `isvalid: false`, never an error. zcashd
    /// answers rather than failing, and the explorer's search box relies on it.
    #[tokio::test]
    async fn validate_address_reports_garbage_as_invalid_not_an_error() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        let got = node
            .validate_address("definitely not an address")
            .await
            .expect("validation answers, it does not fail");
        assert!(!got.isvalid);
        assert!(got.address.is_none());
    }

    #[tokio::test]
    async fn z_validate_address_reports_garbage_as_invalid_not_an_error() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        let got = node
            .z_validate_address("definitely not an address")
            .await
            .expect("validation answers, it does not fail");
        assert!(!got.isvalid);
    }
```

Add `use zcash_protocol::consensus::Network;` to the test module, and update
`engine_with_tip`'s callers: every existing `NodeRpc::new(engine)` in the test
module becomes `NodeRpc::new(engine, Network::MainNetwork)`.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p zaino-noderpc validate_address`
Expected: FAIL — `NodeRpc::new` takes one argument; `validate_address` not found.

- [ ] **Step 4: Give the handler a network**

In `lib.rs`, change the struct and constructor:

```rust
/// Zcash node JSON-RPC handler over a [`NodeRpcService`] engine.
///
/// Carries the network because two served methods — `validateaddress` and
/// `z_validateaddress` — are pure functions of an address and a network, with
/// no chain read at all. The network is a serving parameter, not a capability,
/// so it lives on the adapter.
#[derive(Clone)]
pub struct NodeRpc<S: NodeRpcService> {
    engine: S,
    network: Network,
}

impl<S: NodeRpcService> NodeRpc<S> {
    /// Build the handler over `engine`, validating addresses against `network`.
    pub fn new(engine: S, network: Network) -> Self {
        Self { engine, network }
    }
```

Add `use zcash_protocol::consensus::Network;`.

- [ ] **Step 5: Add the response types**

Append to `wire/response.rs`:

```rust
/// The `validateaddress` response. zcashd reports an unusable address as
/// `isvalid: false` with no other fields, rather than as an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidateAddressResponse {
    /// Whether the address is a transparent address on the queried network.
    pub isvalid: bool,
    /// The address as supplied, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Whether the address is pay-to-script-hash, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isscript: Option<bool>,
}

/// The `z_validateaddress` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZValidateAddressResponse {
    /// Whether the address is one Zaino classifies on the queried network.
    pub isvalid: bool,
    /// The address, re-encoded for the queried network, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// zcashd's address-kind tag: `p2pkh`, `p2sh` or `sapling`.
    #[serde(rename = "address_type", skip_serializing_if = "Option::is_none")]
    pub address_type: Option<String>,
    /// Sapling diversifier as hex, for a Sapling address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diversifier: Option<String>,
    /// Sapling `pk_d` as hex, for a Sapling address.
    #[serde(rename = "diversifiedtransmissionkey", skip_serializing_if = "Option::is_none")]
    pub diversified_transmission_key: Option<String>,
}
```

- [ ] **Step 6: Add the domain→wire conversions**

Append to `wire.rs`:

```rust
/// Render a transparent-address validation for the wire (domain -> wire).
/// Exhaustive by design — a new variant should force a decision here.
pub(crate) fn validated_to_wire(validated: ValidatedAddress) -> ValidateAddressResponse {
    match validated {
        ValidatedAddress::Invalid => ValidateAddressResponse {
            isvalid: false,
            address: None,
            isscript: None,
        },
        ValidatedAddress::Transparent { address, is_script } => ValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            isscript: Some(is_script),
        },
    }
}
```

Write `z_validated_to_wire` the same way, one arm per `ZValidatedAddress`
variant. Read `packages/zaino-address/src/validated.rs` for the exact variant
list and field names before writing it — the Sapling arm carries the
diversifier and `pk_d` as fixed-size byte arrays, which render through
`bytes_to_hex`. Map `P2pkh` to `address_type: "p2pkh"`, `P2sh` to `"p2sh"` and
`Sapling` to `"sapling"`.

Add to `wire.rs`'s imports:

```rust
use zaino_address::{ValidatedAddress, ZValidatedAddress};

use crate::wire::response::{ValidateAddressResponse, ZValidateAddressResponse};
```

- [ ] **Step 7: Write the handlers**

Add to `impl<S: NodeRpcService> NodeRpc<S>`:

```rust
    /// `validateaddress`: classify a transparent address against the serving
    /// network. No chain read — a pure function of the string and the network.
    pub async fn validate_address(
        &self,
        address: &str,
    ) -> Result<ValidateAddressResponse, RpcError> {
        Ok(validated_to_wire(zaino_address::validate_address(
            address.to_owned(),
            &self.network,
        )))
    }

    /// `z_validateaddress`: the deprecated shielded-aware classification.
    pub async fn z_validate_address(
        &self,
        address: &str,
    ) -> Result<ZValidateAddressResponse, RpcError> {
        Ok(z_validated_to_wire(zaino_address::z_validate_address(
            address.to_owned(),
            &self.network,
        )))
    }
```

These are `async` and infallible only to keep one handler shape across the
adapter; the `Result` lets the JSON-RPC impl stay uniform.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p zaino-noderpc`
Expected: PASS — including the pre-existing tests, now updated for the
two-argument `NodeRpc::new`.

- [ ] **Step 9: Add the JSON-RPC methods**

```rust
    #[method(name = "validateaddress")]
    async fn validate_addr(&self, address: String) -> Result<ValidateAddressResponse, ErrorObjectOwned>;

    #[method(name = "z_validateaddress")]
    async fn z_validate_addr(&self, address: String) -> Result<ZValidateAddressResponse, ErrorObjectOwned>;
```

```rust
    async fn validate_addr(&self, address: String) -> Result<ValidateAddressResponse, ErrorObjectOwned> {
        self.validate_address(&address).await.map_err(to_error_object)
    }
    async fn z_validate_addr(&self, address: String) -> Result<ZValidateAddressResponse, ErrorObjectOwned> {
        self.z_validate_address(&address).await.map_err(to_error_object)
    }
```

- [ ] **Step 10: Fix the transport constructor**

`packages/zaino-noderpc/src/transport.rs` builds a `NodeRpc`. Update that call
site for the new two-argument `new`, threading the network in from wherever the
server is configured. Run `cargo test -p zaino-noderpc` to find any other call
site the compiler flags.

- [ ] **Step 11: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-noderpc --all-targets -- -D warnings && cargo test -p zaino-noderpc`
Expected: all pass.

- [ ] **Step 12: Commit**

```bash
git add packages/zaino-noderpc
git commit -m "feat(noderpc): serve validateaddress and z_validateaddress

Both are pure functions of an address string and a network, already implemented
in zaino-address, so neither touches a capability or reads the chain. The handler
now carries the network, because validation is a serving parameter rather than
something a snapshot can answer.

An unusable address answers isvalid: false rather than failing, matching zcashd:
the explorer's search box probes every query string against this and treats an
error differently from a negative answer."
```

---

### Task 5: `z_listunifiedreceivers`

The one explorer method Zaino never committed to that is worth serving: a pure
function over `zcash_address`, belonging beside the other two address functions.

**Files:**
- Create: `packages/zaino-address/src/receivers.rs`
- Modify: `packages/zaino-address/src/lib.rs`
- Modify: `packages/zaino-noderpc/src/lib.rs`
- Modify: `packages/zaino-noderpc/src/wire/response.rs`
- Modify: `packages/zaino-noderpc/src/rpc.rs`

**Interfaces:**
- Consumes: `zcash_address::unified`.
- Produces:
  - `zaino_address::{list_unified_receivers, UnifiedReceivers}`
  - `wire::response::UnifiedReceiversResponse`
  - `NodeRpc::z_list_unified_receivers(&self, &str)`

- [ ] **Step 1: Write the failing test in zaino-address**

Create `packages/zaino-address/src/receivers.rs` with only the test module
first, so the test names the API before it exists:

```rust
#[cfg(test)]
mod tests {
    use super::list_unified_receivers;
    use zcash_protocol::consensus::Network;

    /// A string that is not a unified address has no receivers to list. This is
    /// a definitive answer, not a failure.
    #[test]
    fn a_non_unified_address_has_no_receivers() {
        let got = list_unified_receivers("t1notunified".to_string(), &Network::MainNetwork);
        assert!(got.is_none());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p zaino-address receivers`
Expected: FAIL — `receivers` module is not declared; `list_unified_receivers`
does not exist.

- [ ] **Step 3: Implement the decomposition**

Write the module body above the test in `receivers.rs`:

```rust
//! Unified-address receiver decomposition.
//!
//! `z_listunifiedreceivers` takes a unified address and reports each receiver it
//! bundles, re-encoded as a standalone address. Like the two validation
//! entry points, it reads no chain state: it is a pure function of the address
//! string and the network.

use zcash_protocol::consensus::Parameters;

/// The receivers a unified address bundles, each re-encoded standalone.
///
/// A field is `None` when the unified address carries no receiver of that kind.
/// Only the kinds Zaino re-encodes are modelled; an unknown or unsupported
/// receiver type is omitted rather than reported as an opaque blob.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnifiedReceivers {
    /// Orchard receiver, as a unified address containing only it.
    pub orchard: Option<String>,
    /// Sapling receiver, as a Sapling payment address.
    pub sapling: Option<String>,
    /// Transparent pay-to-public-key-hash receiver.
    pub p2pkh: Option<String>,
    /// Transparent pay-to-script-hash receiver.
    pub p2sh: Option<String>,
}

/// Decompose a unified address into its receivers.
///
/// `None` when `raw_address` is not a unified address for `params`' network —
/// a definitive answer, since the caller asked about a specific string.
pub fn list_unified_receivers<P: Parameters>(
    raw_address: String,
    params: &P,
) -> Option<UnifiedReceivers> {
    // Implementation note for the engineer: parse with
    // `zcash_address::ZcashAddress::try_from_encoded(&raw_address)`, then
    // convert to the unified form. For each item in the unified address's
    // receiver list, re-encode it for `params.network_type()` as a standalone
    // address and place it in the matching field. Consult
    // `packages/zaino-address/src/classify.rs` for how this crate already
    // parses and re-encodes against a `Parameters`, and follow that idiom.
    // Return `None` for any address that does not parse as unified.
    todo!("decompose per the note above")
}
```

Then replace the `todo!` with the real body, following `classify.rs`'s existing
parse-and-re-encode idiom. The task is not complete while a `todo!` remains.

- [ ] **Step 4: Declare and export the module**

In `packages/zaino-address/src/lib.rs`:

```rust
mod receivers;
```

```rust
pub use receivers::{list_unified_receivers, UnifiedReceivers};
```

- [ ] **Step 5: Add a positive test**

Append to `receivers.rs`'s test module, using a real mainnet unified address
string. Take one from `zcash_address`'s own test vectors so the test does not
depend on a fixture this repo would have to maintain:

```rust
    /// A unified address reports each receiver it bundles.
    #[test]
    fn a_unified_address_lists_its_receivers() {
        // Replace with a unified address from zcash_address's test vectors.
        let ua = "<mainnet unified address>".to_string();
        let got = list_unified_receivers(ua, &Network::MainNetwork)
            .expect("a unified address decomposes");
        assert!(
            got.orchard.is_some() || got.sapling.is_some(),
            "a unified address bundles at least one shielded receiver"
        );
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p zaino-address receivers`
Expected: PASS, 2 tests.

- [ ] **Step 7: Add the response type**

Append to `packages/zaino-noderpc/src/wire/response.rs`:

```rust
/// The `z_listunifiedreceivers` response: one field per receiver kind the
/// unified address bundles, absent when it carries none of that kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnifiedReceiversResponse {
    /// Orchard receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchard: Option<String>,
    /// Sapling receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sapling: Option<String>,
    /// Transparent pay-to-public-key-hash receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2pkh: Option<String>,
    /// Transparent pay-to-script-hash receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2sh: Option<String>,
}
```

- [ ] **Step 8: Write the failing adapter test**

Append to `lib.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn listing_receivers_of_a_non_unified_address_is_a_params_error() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.z_list_unified_receivers("t1notunified").await,
            Err(RpcError::InvalidParams(_))
        ));
    }
```

- [ ] **Step 9: Run it to verify it fails**

Run: `cargo test -p zaino-noderpc listing_receivers`
Expected: FAIL — `z_list_unified_receivers` does not exist.

- [ ] **Step 10: Write the handler**

Add to `impl<S: NodeRpcService> NodeRpc<S>`:

```rust
    /// `z_listunifiedreceivers`: the receivers a unified address bundles, each
    /// re-encoded standalone. A pure function of the address and the network.
    ///
    /// An address that is not unified is a parameter error, not an empty
    /// result: the caller asked about a specific string, and reporting "no
    /// receivers" would imply a valid unified address that bundles nothing.
    pub async fn z_list_unified_receivers(
        &self,
        address: &str,
    ) -> Result<UnifiedReceiversResponse, RpcError> {
        let receivers =
            zaino_address::list_unified_receivers(address.to_owned(), &self.network)
                .ok_or_else(|| {
                    RpcError::InvalidParams(format!("{address} is not a unified address"))
                })?;
        Ok(UnifiedReceiversResponse {
            orchard: receivers.orchard,
            sapling: receivers.sapling,
            p2pkh: receivers.p2pkh,
            p2sh: receivers.p2sh,
        })
    }
```

- [ ] **Step 11: Run the tests to verify they pass**

Run: `cargo test -p zaino-noderpc listing_receivers`
Expected: PASS.

- [ ] **Step 12: Add the JSON-RPC method**

```rust
    #[method(name = "z_listunifiedreceivers")]
    async fn z_list_receivers(
        &self,
        address: String,
    ) -> Result<UnifiedReceiversResponse, ErrorObjectOwned>;
```

```rust
    async fn z_list_receivers(
        &self,
        address: String,
    ) -> Result<UnifiedReceiversResponse, ErrorObjectOwned> {
        self.z_list_unified_receivers(&address)
            .await
            .map_err(to_error_object)
    }
```

- [ ] **Step 13: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-address -p zaino-noderpc --all-targets -- -D warnings && cargo test -p zaino-address && cargo test -p zaino-noderpc`
Expected: all pass. Grep the diff for `todo!` and `unimplemented!` — there must
be none.

- [ ] **Step 14: Commit**

```bash
git add packages/zaino-address packages/zaino-noderpc
git commit -m "feat(address): decompose unified addresses, and serve z_listunifiedreceivers

The explorer's address page calls this, and Zaino never committed to it -- but it
is a pure function over zcash_address with no chain read, so it belongs beside
validate_address in zaino-address at near-zero cost.

A non-unified address is a parameter error rather than an empty result:
answering 'no receivers' would imply a valid unified address bundling nothing,
which is a different fact."
```

---

### Task 6: Retire `NodeQueryRelay`, introduce `NodeStatusRead`

The breaking port change. It lands on its own so reviewers see the port churn
without method noise, and so branches rebasing onto it get one clear conflict.

**Files:**
- Delete: `packages/zaino-service/src/node_query.rs`
- Create: `packages/zaino-service/src/node_status.rs`
- Modify: `packages/zaino-service/src/controls.rs`
- Modify: `packages/zaino-service/src/lib.rs`
- Modify: `packages/zaino-service/src/capability.rs`
- Modify: `packages/zaino-service/src/use_cases/node_rpc.rs`
- Modify: `packages/zaino-service/src/testing.rs`
- Modify: `packages/zaino-indexes/src/capabilities.rs`

**Interfaces:**
- Consumes: `zaino_primitives::types::rpc::{MiningInfo, NodeInfo, PeerInfo}`,
  `zaino_primitives::types::{Difficulty, Height}`.
- Produces:
  - `zaino_service::{NodeStatusRead, NodeStatusError}` with `node_info`,
    `mining_info`, `peer_info`, `network_sol_ps`
  - `Capability::NodeStatus`
  - `NodeRpcService` bound on `NodeStatusRead` instead of `NodeQueryRelay`
  - `NodeQuery`, `NodeQueryAnswer`, `NodeQueryRelay` no longer exist

- [ ] **Step 1: Write the failing compile-time test**

In `packages/zaino-service/src/testing.rs`'s `mod tests`, the existing
`mock_satisfies_every_use_case` is the test: it asserts
`MockIndexerService: NodeRpcService`. Add the capability assertion beside it:

```rust
    /// Every capability has a serviceability answer, including the node-status
    /// passthrough bundle.
    #[test]
    fn node_status_is_a_capability() {
        use crate::{Answerable, Capability, Serviceable};
        let engine = MockIndexerService::new(MockChain::default());
        assert_eq!(
            engine.serviceability().get(Capability::NodeStatus),
            Answerable::NotYet
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p zaino-service --features testing node_status`
Expected: FAIL — `Capability::NodeStatus` does not exist.

- [ ] **Step 3: Create the node-status port**

Create `packages/zaino-service/src/node_status.rs`:

```rust
//! Node-operator status, read from the validator.
//!
//! Four facts about the node rather than the chain: its identity and
//! connections, its mining view, its peers, and the network solution rate.
//! Zaino indexes none of them, so all four are answered by passthrough.
//!
//! They are **typed**, not opaque: every `zaino-source` port beneath this one
//! already returns a domain type, so relaying a rendered string would discard
//! type information the layer below holds.
//!
//! One capability covers all four. They share the same availability — always
//! `Answerable::Live`, never height-bounded — so separate capability variants
//! would carry no information this one does not.

use std::future::Future;

use zaino_primitives::types::rpc::{MiningInfo, NodeInfo, PeerInfo};
use zaino_primitives::types::Height;

/// Why a node-status read could not be answered.
///
/// Distinct from [`Transient`](crate::error::Transient), which is about
/// acquiring a snapshot: no snapshot is involved here. The distinction that
/// matters to a caller is whether the validator is *starting*, in which case
/// the same request succeeds shortly, or *unreachable*, which is a different
/// problem with a different fix.
///
/// The cause is held as a boxed `#[source]` rather than a `zaino-source` type
/// because this crate is the inner driving port and must not depend on the
/// driven one. Boxing keeps the chain intact without the dependency.
#[derive(Debug, thiserror::Error)]
pub enum NodeStatusError {
    /// The validator is running but not yet ready to describe itself.
    #[error("validator not ready")]
    NotReady,
    /// The validator could not be reached, or its answer was unusable.
    #[error("validator unreachable")]
    Unreachable {
        /// The underlying source-layer failure.
        #[source]
        cause: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl NodeStatusError {
    /// An unreachable validator, preserving `cause` in the source chain.
    pub fn unreachable<E>(cause: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Unreachable {
            cause: Box::new(cause),
        }
    }
}

/// Node-operator status, relayed from the validator.
///
/// A control rather than a snapshot read: these answers come from the source
/// live, and nothing pins them to a chain view.
pub trait NodeStatusRead: Send + Sync {
    /// `getinfo`: version, connections, fee floors and health.
    fn node_info(&self) -> impl Future<Output = Result<NodeInfo, NodeStatusError>> + Send;

    /// `getmininginfo`: the validator's mining view.
    fn mining_info(&self) -> impl Future<Output = Result<MiningInfo, NodeStatusError>> + Send;

    /// `getpeerinfo`: the validator's connected peers.
    fn peer_info(&self) -> impl Future<Output = Result<Vec<PeerInfo>, NodeStatusError>> + Send;

    /// `getnetworksolps`: the network solution rate in solutions per second,
    /// averaged over `blocks` ending at `height`. `None` for either asks the
    /// validator for its own default — its window, and its tip.
    fn network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> impl Future<Output = Result<u64, NodeStatusError>> + Send;
}
```

- [ ] **Step 4: Delete the opaque relay**

```bash
git rm packages/zaino-service/src/node_query.rs
```

In `packages/zaino-service/src/controls.rs`, delete the `NodeQueryRelay` trait
and remove `NodeQuery`/`NodeQueryAnswer` from its `use crate::{...}` list.

In `packages/zaino-service/src/lib.rs`, replace `mod node_query;` with
`mod node_status;`, and replace the `pub use node_query::{NodeQuery,
NodeQueryAnswer};` export with `pub use node_status::{NodeStatusError, NodeStatusRead};`. Remove
`NodeQueryRelay` from the `controls` re-export list.

- [ ] **Step 5: Add the capability variant**

In `packages/zaino-service/src/capability.rs`, add to `enum Capability`:

```rust
    /// Node-operator status — served by the validator (no local index).
    NodeStatus,
```

- [ ] **Step 6: Classify it as having no local index**

In `packages/zaino-indexes/src/capabilities.rs`, add `Capability::NodeStatus` to
the no-local-index arm of `capability_indexes`:

```rust
        Capability::RawTransaction
        | Capability::Treestate
        | Capability::SubtreeRoots
        | Capability::Mempool
        | Capability::Broadcast
        | Capability::NodeStatus
        | Capability::ReportedUpgrades => &[],
```

- [ ] **Step 7: Rebind the use case**

In `packages/zaino-service/src/use_cases/node_rpc.rs`, replace `NodeQueryRelay`
with `NodeStatusRead` in the import, the trait bound, the blanket `where`
clause, and the doc comment's description of the relay.

- [ ] **Step 8: Reimplement on the mock**

In `packages/zaino-service/src/testing.rs`, delete `impl NodeQueryRelay for
MockIndexerService` and add:

```rust
impl NodeStatusRead for MockIndexerService {
    async fn node_info(&self) -> Result<NodeInfo, NodeStatusError> {
        // The mock has no validator behind it, so "not ready" is the honest
        // answer — and it is the arm the adapter must surface as retryable.
        Err(NodeStatusError::NotReady)
    }
    async fn mining_info(&self) -> Result<MiningInfo, NodeStatusError> {
        Err(NodeStatusError::NotReady)
    }
    async fn peer_info(&self) -> Result<Vec<PeerInfo>, NodeStatusError> {
        Ok(Vec::new())
    }
    async fn network_sol_ps(
        &self,
        _blocks: Option<u32>,
        _height: Option<Height>,
    ) -> Result<u64, NodeStatusError> {
        Ok(0)
    }
}
```

Update the imports: drop `NodeQuery`/`NodeQueryAnswer`/`NodeQueryRelay`, add
`NodeStatusRead`, `NodeStatusError` and
`zaino_primitives::types::rpc::{MiningInfo, NodeInfo, PeerInfo}`.

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test -p zaino-service --features testing`
Expected: PASS — including `mock_satisfies_every_use_case`, which now proves the
mock satisfies `NodeRpcService` through `NodeStatusRead`.

- [ ] **Step 10: Fix the engine and adapter call sites**

`packages/zaino-core/src/engine.rs` implements `NodeQueryRelay`, and
`packages/zaino-noderpc/src/lib.rs`'s `get_mining_info` calls
`relay_node_query`. Both now fail to compile. In this task, delete the engine's
`NodeQueryRelay` impl and the adapter's `get_mining_info` handler plus its
`getmininginfo` JSON-RPC method and test — Task 7 reimplements the engine side
and Task 8 the adapter side, typed.

Run: `cargo test -p zaino-core -p zaino-noderpc`
Expected: both compile and pass.

- [ ] **Step 11: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-service -p zaino-indexes -p zaino-core -p zaino-noderpc --all-targets -- -D warnings && cargo test -p zaino-service --features testing && cargo test -p zaino-indexes && cargo test -p zaino-core && cargo test -p zaino-noderpc`
Expected: all pass.

- [ ] **Step 12: Commit**

```bash
git add -A packages/zaino-service packages/zaino-indexes packages/zaino-core packages/zaino-noderpc
git commit -m "refactor!: replace the opaque node-query relay with typed NodeStatusRead

NodeQueryRelay answered a three-variant NodeQuery with NodeQueryAnswer(String),
on the grounds that Zaino does not model these facts. The driven ports beneath it
already do: get_node_info, get_mining_info, get_peer_info and get_network_sol_ps
each return a domain type. Relaying them as strings discarded type information
one layer below already held.

NodeStatusRead names the five reads with their real types. One Capability variant
covers all of them, because they share the same availability -- always Live,
never height-bounded -- so separate variants would carry no information.

BREAKING: NodeQuery, NodeQueryAnswer and NodeQueryRelay are removed. The engine
impl and the adapter's getmininginfo are reinstated typed in the two commits that
follow."
```

---

### Task 7: Engine `NodeStatusRead` over the typed source ports

**Files:**
- Modify: `packages/zaino-core/src/passthrough.rs`
- Modify: `packages/zaino-core/src/engine.rs`

**Interfaces:**
- Consumes: `zaino_source::{GetNodeInfo, GetMiningInfo, GetPeerInfo,
  GetNetworkSolPs, GetDifficulty}` and their `*Error` types;
  `zaino_service::NodeStatusRead`.
- Produces: `PassthroughProvider::{node_info, mining_info, peer_info,
  network_sol_ps, difficulty}`; `impl NodeStatusRead for Engine<..>`.

- [ ] **Step 1: Write the failing test**

Append to `packages/zaino-core/src/passthrough.rs`'s test module (create one if
absent, following the pattern in a sibling `zaino-core` module):

```rust
#[cfg(test)]
mod tests {
    use super::PassthroughProvider;
    use zaino_source::mock::MockChain as SourceMock;

    /// Review Focus 1: an unreachable validator is an error, never a
    /// zero-valued success. The explorer's warmers keep their previous cache on
    /// an error and would otherwise cache a wrong value for 15 seconds.
    #[tokio::test]
    async fn an_unreachable_validator_errors_rather_than_answering_zero() {
        let source = SourceMock::new().fail_next(1, zaino_source::FailureMode::Unavailable);
        let provider = PassthroughProvider::new(source);
        assert!(provider.network_sol_ps(None, None).await.is_err());
    }
}
```

Read `packages/zaino-source/src/mock.rs` for the exact `fail_next` and
`FailureMode` API and adjust the call to match it.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p zaino-core unreachable_validator`
Expected: FAIL — `network_sol_ps` does not exist on `PassthroughProvider`.

- [ ] **Step 3: Add the passthrough methods**

Append to `packages/zaino-core/src/passthrough.rs`, one `impl` block per port,
following the existing `treestate`/`raw_transaction` shape exactly. Every arm
maps `SourceError::NonDomain` and `SourceError::Unavailable` to `Transient`, and
the domain error to `Transient` as well — these are validator facts with no
Zaino-side domain miss:

```rust
impl<Src> PassthroughProvider<Src>
where
    Src: GetNodeInfo,
{
    /// The validator's self-description, live. A validator that is not ready is
    /// transient: it becomes ready.
    pub(crate) async fn node_info(&self) -> Result<NodeInfo, Transient> {
        match self.source.get_node_info().await {
            Ok(info) => Ok(info),
            Err(SourceError::Domain(GetNodeInfoError::NotReady)) => {
                Err(Transient::new("validator not ready"))
            }
            Err(SourceError::NonDomain(cause)) => {
                Err(Transient::new(format!("validator unavailable: {cause}")))
            }
            Err(SourceError::Unavailable(cause)) => {
                Err(Transient::new(format!("validator unavailable: {cause}")))
            }
        }
    }
}
```

Write `mining_info`, `peer_info`, `network_sol_ps` and `difficulty` the same
way. `network_sol_ps` forwards its `blocks` and `height` arguments. Read each
port's `*Error` enum in `packages/zaino-source/src/get_*.rs` and write one arm
per variant — no wildcard. Extend the file's imports with the five port traits,
their error types, and `NodeInfo`/`MiningInfo`/`PeerInfo`/`Difficulty`/`Height`.

Use `Transient`'s real constructor from
`packages/zaino-service/src/error.rs`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p zaino-core unreachable_validator`
Expected: PASS.

- [ ] **Step 5: Implement the port on the engine**

In `packages/zaino-core/src/engine.rs`, where the deleted `NodeQueryRelay` impl
was, add:

```rust
/// Always passthrough: these are facts about the validator, not the chain, so
/// no index backs any of them.
impl<F, N, Src, R> NodeStatusRead for Engine<F, N, Src, R>
where
    Src: GetNodeInfo + GetMiningInfo + GetPeerInfo + GetNetworkSolPs + GetDifficulty,
{
    async fn node_info(&self) -> Result<NodeInfo, Transient> {
        self.passthrough().node_info().await
    }
    async fn mining_info(&self) -> Result<MiningInfo, Transient> {
        self.passthrough().mining_info().await
    }
    async fn peer_info(&self) -> Result<Vec<PeerInfo>, Transient> {
        self.passthrough().peer_info().await
    }
    async fn network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> Result<u64, Transient> {
        self.passthrough().network_sol_ps(blocks, height).await
    }
    async fn difficulty(&self) -> Result<Difficulty, Transient> {
        self.passthrough().difficulty().await
    }
}
```

Match the generic parameters and `where` clause to the other control impls in
that file — read the deleted `NodeQueryRelay` impl in `git show HEAD~1` for the
exact bound shape, and add the accessor for the passthrough provider if
`Engine` names it differently.

- [ ] **Step 6: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-core --all-targets -- -D warnings && cargo test -p zaino-core`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add packages/zaino-core
git commit -m "feat(core): implement NodeStatusRead as typed validator passthrough

Five node-operator facts relayed from the source's already-typed ports, replacing
the string-rendering relay. Each keeps the provider's established error shape: a
validator that is not ready is transient, because it becomes ready.

An unreachable validator errors rather than answering a zero value. A consumer
that caches successes and ignores errors -- which is how the explorer's metric
warmers work -- would otherwise cache a wrong number."
```

---

### Task 8: Adapter — `getinfo`, `getmininginfo`, `getpeerinfo`, `getnetworksolps`

**Files:**
- Modify: `packages/zaino-noderpc/src/wire/response.rs`
- Modify: `packages/zaino-noderpc/src/wire.rs`
- Modify: `packages/zaino-noderpc/src/lib.rs`
- Modify: `packages/zaino-noderpc/src/rpc.rs`

**Interfaces:**
- Consumes: `zaino_service::NodeStatusRead`.
- Produces: `wire::response::{NodeInfoResponse, MiningInfoResponse,
  PeerInfoEntry}`; `NodeRpc::{get_info, get_mining_info, get_peer_info,
  get_network_sol_ps}`.

- [ ] **Step 1: Write the failing tests**

Append to `lib.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn peer_info_and_network_solps_read_the_node_status_port() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(node.get_peer_info().await.expect("peers").is_empty());
        assert_eq!(
            node.get_network_sol_ps(None, None).await.expect("solps"),
            0
        );
    }

    /// Review Focus 1 at the adapter boundary: a transient becomes an RPC
    /// error, not a default-valued success.
    #[tokio::test]
    async fn an_unavailable_node_info_is_an_rpc_error() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_info().await,
            Err(RpcError::Unavailable(_))
        ));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p zaino-noderpc node_status network_solps unavailable_node_info`
Expected: FAIL — the handlers do not exist.

- [ ] **Step 3: Add the response types**

Append to `wire/response.rs`. Read
`packages/zaino-primitives/src/types/rpc/{node_info,mining_info,peer_info}.rs`
for the exact field sets and mirror them, using zcashd's wire field names:

```rust
/// The `getinfo` response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeInfoResponse {
    /// Validator version.
    pub version: u64,
    /// Build string.
    pub build: String,
    /// Sub-version string.
    pub subversion: String,
    /// Peer protocol version.
    pub protocolversion: u32,
    /// Best block height.
    pub blocks: u32,
    /// Peer connection count.
    pub connections: u64,
    /// Current proof-of-work difficulty.
    pub difficulty: f64,
    /// Whether this is a testnet node.
    pub testnet: bool,
    /// Configured proxy, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Configured transaction fee, in zatoshis.
    pub paytxfee: u64,
    /// Minimum relay fee, in zatoshis.
    pub relayfee: u64,
    /// Current error message, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<String>,
    /// Timestamp of the current error, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errorstimestamp: Option<i64>,
}
```

Add `MiningInfoResponse` and `PeerInfoEntry` the same way, one wire field per
domain field.

- [ ] **Step 4: Add the conversions**

Append to `wire.rs` a `node_info_to_wire`, `mining_info_to_wire` and
`peer_info_to_wire`, each a total field-by-field mapping. Zatoshi fields render
through `as_u64()`; `Height` renders through `u32::from(..)`.

- [ ] **Step 5: Write the handlers**

Add to `impl<S: NodeRpcService> NodeRpc<S>`:

```rust
    /// `getinfo`: the validator's self-description, relayed.
    pub async fn get_info(&self) -> Result<NodeInfoResponse, RpcError> {
        Ok(node_info_to_wire(self.engine.node_info().await?))
    }

    /// `getmininginfo`: the validator's mining view, relayed. Not indexed.
    pub async fn get_mining_info(&self) -> Result<MiningInfoResponse, RpcError> {
        Ok(mining_info_to_wire(self.engine.mining_info().await?))
    }

    /// `getpeerinfo`: the validator's connected peers, relayed. Not indexed.
    pub async fn get_peer_info(&self) -> Result<Vec<PeerInfoEntry>, RpcError> {
        Ok(self
            .engine
            .peer_info()
            .await?
            .into_iter()
            .map(peer_info_to_wire)
            .collect())
    }

    /// `getnetworksolps`: the network solution rate, relayed. `blocks` and
    /// `height` are forwarded as given, so `None` means the validator's own
    /// defaults rather than a value this adapter invents.
    pub async fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<u32>,
    ) -> Result<u64, RpcError> {
        let height = height
            .map(Height::try_from)
            .transpose()
            .map_err(|_| RpcError::InvalidParams("height is not a valid height".into()))?;
        Ok(self.engine.network_sol_ps(blocks, height).await?)
    }
```

Add `use zaino_service::NodeStatusRead;` and import the three conversions and
three response types.

`RpcError::Unavailable` already has `#[from] Transient`, so `?` works on all
four.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p zaino-noderpc node_status network_solps unavailable_node_info`
Expected: PASS.

- [ ] **Step 7: Add the JSON-RPC methods**

Add four methods to the `#[rpc(server)]` trait and impl, mirroring Task 1 step
12: `getinfo`, `getmininginfo`, `getpeerinfo`, and `getnetworksolps` (which
takes `blocks: Option<u32>, height: Option<u32>` — the explorer sends
`[120, -1]`, so a negative height arrives as a deserialization error; accept
`Option<i64>` for `height` and treat any negative value as `None`, matching
zcashd's "-1 means the tip" convention).

- [ ] **Step 8: Verify lint and tests**

Run: `makers fmt && cargo clippy -p zaino-noderpc --all-targets -- -D warnings && cargo test -p zaino-noderpc`
Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add packages/zaino-noderpc
git commit -m "feat(noderpc): serve getinfo, getmininginfo, getpeerinfo and getnetworksolps

Four of the explorer's five metric warmers, over the typed NodeStatusRead port.
Each renders the domain type field-by-field into zcashd's wire shape in the
adapter.

getnetworksolps accepts a negative height as 'the tip', which is what zcashd
documents and what the explorer's client sends: it calls with [120, -1]."
```

---

### Task 9: `MempoolListing` — `getrawmempool` and `getmempoolinfo`

The last of slice 2. The passthrough provider already has `mempool_txids`; this
adds the size/bytes listing beside it and the port that exposes both.

**Files:**
- Create: `packages/zaino-service/src/mempool_listing.rs`
- Modify: `packages/zaino-service/src/lib.rs`
- Modify: `packages/zaino-service/src/use_cases/node_rpc.rs`
- Modify: `packages/zaino-service/src/testing.rs`
- Modify: `packages/zaino-core/src/passthrough.rs`
- Modify: `packages/zaino-core/src/engine.rs`
- Modify: `packages/zaino-noderpc/src/{lib.rs,rpc.rs,wire.rs,wire/response.rs}`

**Interfaces:**
- Consumes: `zaino_source::{GetMempoolTxids, GetMempoolMetadata}`;
  `PassthroughProvider::mempool_txids` (already present).
- Produces:
  - `zaino_service::{MempoolListing, MempoolSummary}`
  - `wire::response::MempoolInfoResponse`
  - `NodeRpc::{get_raw_mempool, get_mempool_info}`

- [ ] **Step 1: Write the failing test**

Append to `packages/zaino-noderpc/src/lib.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn raw_mempool_lists_txids_and_info_counts_them() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        let listed = node.get_raw_mempool().await.expect("mempool listing");
        let info = node.get_mempool_info().await.expect("mempool info");
        assert_eq!(
            u64::try_from(listed.len()).expect("fits"),
            info.size,
            "the listing and the count must agree"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p zaino-noderpc raw_mempool_lists`
Expected: FAIL — the handlers do not exist.

- [ ] **Step 3: Create the port**

Create `packages/zaino-service/src/mempool_listing.rs`:

```rust
//! One-shot mempool listing, for the node-RPC shape.
//!
//! Distinct from [`MempoolSubscribe`](crate::MempoolSubscribe), which is a
//! stream a wallet follows: node RPC asks "what is in the mempool right now",
//! once, and gets an answer. Both come from the source that serves the mempool,
//! never from a finalised secondary, which holds none.

use std::future::Future;

use zaino_primitives::types::TransactionId;

use crate::error::MempoolReadError;

/// Aggregate mempool statistics — the domain behind `getmempoolinfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MempoolSummary {
    /// Number of transactions in the mempool.
    pub size: u64,
    /// Total serialized size of those transactions, in bytes.
    pub bytes: u64,
}

/// A point-in-time mempool listing.
pub trait MempoolListing: Send + Sync {
    /// `getrawmempool`: the txids currently in the mempool.
    fn mempool_txids(
        &self,
    ) -> impl Future<Output = Result<Vec<TransactionId>, MempoolReadError>> + Send;

    /// `getmempoolinfo`: the count and total size of those transactions.
    fn mempool_summary(
        &self,
    ) -> impl Future<Output = Result<MempoolSummary, MempoolReadError>> + Send;
}
```

- [ ] **Step 4: Export it and add it to the use case**

In `packages/zaino-service/src/lib.rs`: `mod mempool_listing;` and
`pub use mempool_listing::{MempoolListing, MempoolSummary};`.

In `packages/zaino-service/src/use_cases/node_rpc.rs`, add `MempoolListing` to
the import, the `NodeRpcService` supertrait list, and the blanket `where`
clause.

- [ ] **Step 5: Implement it on the mock**

In `packages/zaino-service/src/testing.rs`:

```rust
impl MempoolListing for MockIndexerService {
    async fn mempool_txids(&self) -> Result<Vec<TransactionId>, MempoolReadError> {
        Ok(self.current().mempool.iter().map(|tx| tx.txid).collect())
    }
    async fn mempool_summary(&self) -> Result<MempoolSummary, MempoolReadError> {
        let size = u64::try_from(self.current().mempool.len())
            .map_err(|_| MempoolReadError::Transient("mempool length overflows u64".into()))?;
        // The mock's listing carries no bytes, so the honest total is zero.
        Ok(MempoolSummary { size, bytes: 0 })
    }
}
```

Check `MempoolTx`'s field for the txid in `packages/zaino-service/src/` and use
its real name.

- [ ] **Step 6: Add the summary passthrough**

In `packages/zaino-core/src/passthrough.rs`, add beside the existing
`mempool_txids`:

```rust
impl<Src> PassthroughProvider<Src>
where
    Src: GetMempoolMetadata,
{
    /// Count and total size of the validator's mempool, live. A validator that
    /// exposes no mempool is served as an *empty* mempool, matching
    /// [`mempool_txids`](Self::mempool_txids); a transport failure is transient.
    pub(crate) async fn mempool_summary(&self) -> Result<MempoolSummary, MempoolReadError> {
        match self.source.get_mempool_metadata().await {
            Ok(entries) => {
                let size = u64::try_from(entries.len()).map_err(|_| {
                    MempoolReadError::Transient("mempool length overflows u64".into())
                })?;
                Ok(MempoolSummary { size, bytes: 0 })
            }
            Err(SourceError::Domain(GetMempoolMetadataError::Unavailable)) => {
                Ok(MempoolSummary::default())
            }
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}
```

`bytes` is zero because `MempoolTxMeta` carries no serialized size. Leave the
doc comment above saying so, and note that a real byte total needs a source port
that reports it.

- [ ] **Step 7: Implement the port on the engine**

In `packages/zaino-core/src/engine.rs`, add an `impl MempoolListing for
Engine<..>` forwarding both methods to the passthrough provider, with
`Src: GetMempoolTxids + GetMempoolMetadata` added to the bound. Match the
generic shape of the neighbouring `MempoolContent` impl.

- [ ] **Step 8: Add the response type and handlers**

Append to `packages/zaino-noderpc/src/wire/response.rs`:

```rust
/// The `getmempoolinfo` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MempoolInfoResponse {
    /// Number of transactions in the mempool.
    pub size: u64,
    /// Total serialized size of those transactions, in bytes.
    pub bytes: u64,
}
```

Add to `impl<S: NodeRpcService> NodeRpc<S>`:

```rust
    /// `getrawmempool`: the txids currently in the mempool, as hex.
    pub async fn get_raw_mempool(&self) -> Result<Vec<String>, RpcError> {
        Ok(self
            .engine
            .mempool_txids()
            .await?
            .into_iter()
            .map(|txid| to_hex(txid.into()))
            .collect())
    }

    /// `getmempoolinfo`: the count and total size of the mempool.
    pub async fn get_mempool_info(&self) -> Result<MempoolInfoResponse, RpcError> {
        let summary = self.engine.mempool_summary().await?;
        Ok(MempoolInfoResponse {
            size: summary.size,
            bytes: summary.bytes,
        })
    }
```

Add `RpcError::MempoolRead(#[from] MempoolReadError)` to `error.rs` and its arms
to `to_error_object` — `Transient` is an internal error, matching Review Focus 1.
Add `use zaino_service::MempoolListing;` to `lib.rs`.

- [ ] **Step 9: Run the test to verify it passes**

Run: `cargo test -p zaino-noderpc raw_mempool_lists`
Expected: PASS.

- [ ] **Step 10: Add the JSON-RPC methods**

Add `getrawmempool` (no params, returns `Vec<String>`) and `getmempoolinfo` (no
params, returns `MempoolInfoResponse`) to the `#[rpc(server)]` trait and impl.

- [ ] **Step 11: Verify lint and the full slice**

Run: `makers fmt && cargo clippy -p zaino-service -p zaino-core -p zaino-noderpc --all-targets -- -D warnings && cargo test -p zaino-service --features testing && cargo test -p zaino-core && cargo test -p zaino-noderpc && cargo test -p zaino-indexes`
Expected: all pass.

- [ ] **Step 12: Commit**

```bash
git add packages/zaino-service packages/zaino-core packages/zaino-noderpc
git commit -m "feat: add MempoolListing, and serve getrawmempool and getmempoolinfo

Node RPC asks what is in the mempool once and gets an answer, where
MempoolSubscribe is a stream a wallet follows -- a different question, so a
separate port rather than a one-shot bolted onto the subscription.

getmempoolinfo reports bytes as zero: MempoolTxMeta carries no serialized size,
and a real total needs a source port that reports one. Reporting zero is visible;
omitting the field or inventing a total would not be.

Completes slice 2. The explorer's five metric warmers and its address and
transaction-search paths are now served; its block pages wait on slices 4 and 5."
```

---

## Self-Review

**Spec coverage.** Slice 1 of the spec is Tasks 1–5 (`getaddressbalance`,
`getaddressdeltas`, `getrawtransaction` verbosity 0, `validateaddress`,
`z_validateaddress`, `z_listunifiedreceivers` — all six). Slice 2 is Tasks 6–9
(`NodeQueryRelay` retirement, `NodeStatusRead`, the four node-status methods,
`MempoolListing` with its two methods). The spec's slices 3–5 are **not** in
this plan; they need a second plan, which is noted in the handoff below.

**Placeholders.** One deliberate `todo!()` appears in Task 5 Step 3, inside the
step that immediately requires replacing it, because the `zcash_address` unified
re-encoding idiom must be read from `classify.rs` rather than guessed. Step 13 of
that task greps for `todo!` as a gate. Task 8 Step 3 and Task 9 Step 7 direct the
engineer to mirror a field set from a named file rather than reproducing a
24-field struct whose exact contents were not read — each names the file and the
rule.

**Type consistency.** `full_range` is defined once (Task 1) and used by Task 2.
`AddressesParam` (Task 1) and `AddressDeltasParam` (Task 2) are separate types
because `getaddressdeltas` carries range and `chainInfo` fields.
`NodeRpc::new` gains its second argument in Task 4, and Task 4 Step 10 fixes the
transport call site. `bytes_to_hex` (Task 3) and `to_hex` (pre-existing,
`[u8; 32]`) are distinct and both used. `MempoolSummary` is defined in Task 9
Step 3 and used in Steps 5, 6 and 8.

**Review Focus coverage.** All five have tests: (1) Task 7 Step 1 and Task 8
Step 1; (2) Task 1 Step 3; (3) Task 2 Step 2; (4) Task 3 Step 2; (5) Task 4
Step 2.
