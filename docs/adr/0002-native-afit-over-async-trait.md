# Native `async fn` in traits over the `async-trait` macro

## Status

accepted

## Context and decision

`zaino-state` annotates its async trait definitions with `#[async_trait]`.
That macro rewrites each `async fn` into a method returning a boxed
`Pin<Box<dyn Future + Send>>`. It predates the language feature that makes the
macro unnecessary: native `async fn` in traits — "AFIT", built on
return-position `impl Trait` in traits ("RPITIT") — which stabilised in Rust
1.75.0 (December 2023). This crate is built on a toolchain pinned to 1.95.0
(`rust-toolchain.toml`), so the feature has been available the whole time.

We will migrate `zaino-state`'s own trait definitions off `#[async_trait]` to
native AFIT, writing the `Send` bound the macro used to supply by hand through
a one-line trait alias (see "The pattern"). Impl blocks keep their ergonomic
`async fn` form unchanged.

The migration requires no MSRV (minimum supported Rust version) change: the
pattern needs only RPITIT plus an
auto-trait bound on the returned `impl Future`, both stable since 1.75.0 — far
below the 1.75-and-up that the 1.95.0 pin already guarantees. (The repository
declares no `rust-version`; the toolchain pin is the de facto floor.)

## What `#[async_trait]` actually buys here

Two distinct capabilities are commonly attributed to the macro; only one is in
use in this crate.

1. **Object safety (`dyn Trait`).** Not needed. A mechanical check confirms it:
   strip `#[async_trait]` from every site in the crate, convert to native AFIT,
   and compile. Native AFIT traits are not object-safe, so any `dyn`-using call
   site would surface as error E0038 ("the trait cannot be made into an
   object"). The result is **zero** E0038 across the crate — no call site uses
   any of these traits as a trait object. Three of the traits (`DbWrite`,
   `DbCore`, `Migration`) are already not object-safe today — they carry a
   generic method or associated consts — so the committed tree compiling is
   itself proof those have no `dyn` users.

2. **`Send`-bounded futures.** This *is* what the macro provides here. The
   futures returned by these methods are awaited across task-spawn boundaries,
   so they must be `Send`. `#[async_trait]` boxes them as
   `Pin<Box<dyn Future + Send>>`, supplying the bound implicitly. A naive strip
   (removing the attribute without restoring `Send`) fails to compile with 15
   `Send`-related errors (E0277, "future cannot be sent between threads
   safely") and no E0038 — confirming `Send`, not object safety, is the load
   the macro carries.

The migration therefore must preserve the `Send` bound and need not preserve
object safety.

## The pattern

A supertrait alias with a blanket impl names "a `Send` future of output `T`"
once for the whole crate. The blanket-impl form is stable Rust; the `trait Foo
= ...` alias *syntax* and type-alias-`impl Trait` are not, and are deliberately
avoided.

```rust
use std::future::Future;

/// Any `Send` future with output `T`.
trait SendFut<T>: Future<Output = T> + Send {}
impl<T, F: Future<Output = T> + Send> SendFut<T> for F {}
```

Each trait-definition method then desugars its `async fn` to a method returning
`impl SendFut<T>`:

```rust
// before
#[async_trait]
pub trait BlockchainSource: Clone + Send + Sync + 'static {
    async fn get_block(&self, id: HashOrHeight)
        -> BlockchainSourceResult<Option<Arc<Block>>>;
}

// after
pub trait BlockchainSource: Clone + Send + Sync + 'static {
    fn get_block(&self, id: HashOrHeight)
        -> impl SendFut<BlockchainSourceResult<Option<Arc<Block>>>>;
}
```

Impl blocks are untouched — plain `async fn get_block(&self, ...) -> ...`
satisfies `impl SendFut<...>` as long as the body is `Send`, which the compiler
verifies at the impl site.

Folding `+ Send` into the alias is the reason to prefer it over the raw
`impl Future<Output = T> + Send` longhand: the bound lives in one place instead
of being re-typed on every method, so it cannot be silently omitted on a single
method and reintroduce the `Send` failures above.

## What the migration makes explicit

Independent of the dependency argument below, rewriting the signatures by hand
is an improvement because it replaces three implicit behaviours of
`#[async_trait]` with explicit ones in the trait's signature:

- **The `Send` bound becomes explicit.** `#[async_trait]` applies `+ Send` to
  every boxed future uniformly and invisibly. Writing `impl SendFut<T>` states
  the bound where the method is declared.
- **Return types become concrete trait implementors, not trait objects.**
  `#[async_trait]` returns `Pin<Box<dyn Future + Send>>` — a heap-allocated
  trait object dispatched dynamically, one allocation per call. Native AFIT
  returns an opaque concrete future (`impl Future`): static dispatch, no
  per-call boxing. The zero-E0038 check proves no call site depended on the
  trait-object form, so the switch loses nothing.
- **It surfaces which methods actually relied on the implicit `Send`.** Because
  the macro supplied the bound wholesale, it was invisible which futures
  genuinely cross a spawn boundary and which never needed `Send` at all. Making
  it per-method documents that requirement in the signature; the 15 `Send`
  errors from a naive strip enumerate exactly the methods that were leveraging
  the previously-implicit bound.

## Why migrate now, given no dependency is removed

Removing `#[async_trait]` from this crate does **not** drop the `async-trait`
crate from the build. It is pulled in by three independent roots — `jsonrpsee-core`
and `tonic` (both transitive, via `zebra-rpc` and `zaino-proto`), and
`zaino-state`'s own direct dependency. A transitive crate leaves the graph only
when every holder releases it; `jsonrpsee` and `tonic` keep compiling it
regardless of what this crate does. So there is no build-time or binary-size
win today.

The decision is justified on positioning, not on an immediate dependency count:

- **Off the critical path.** This crate's direct use is the one term in that
  removal condition that the project controls. While it remains, the project is
  a co-owner of the `async-trait` edge: even if both upstreams migrated to
  native AFIT, the crate would stay solely because of this code, and removing
  it would then be net-new migration work done reactively. Migrating now makes
  the project a pure beneficiary — the day `tonic` and `jsonrpsee` ship native
  AFIT, `async-trait` falls out of the tree with no further work here.
- **Smaller declared dependency surface today, and less version-skew exposure.**
  Dropping the direct `async-trait` declaration means the project no longer
  pins or constrains that crate's version resolution — so it cannot be the
  source of a version skew against what `tonic` or `jsonrpsee` want, and a
  future `async-trait` semver bump or advisory is the upstreams' concern, not a
  direct edge this crate has to track. The project is also no longer flagged as
  a direct user by supply-chain audit tooling (`cargo audit` / `cargo deny`),
  independent of the transitive copy still built for the upstreams.

This is a deliberate trade: a one-time, bounded migration cost paid now in
exchange for a deferred dependency-removal payoff that is contingent on upstream
timelines, plus the immediate audit-surface reduction. The `SendFut` alias
keeps the migration cost low and the `Send` contract unforgettable, which is
what tips the trade.

## Scope: what migrates and what does not

In scope — the trait *definitions* in `zaino-state` and their impls (the
attribute is removed from both; only the definition signatures are rewritten,
impls keep `async fn`).

Out of scope, left on `async-trait` or its hand-written desugaring:

- The tonic-generated `CompactTxStreamer` trait in `zaino-proto`
  (`@generated` prost/tonic output; the attribute is emitted by codegen, not
  hand-authored, and would return on regeneration).
- The `jsonrpsee::core::async_trait` impl of the jsonrpse `RpcServer` trait in
  `zaino-serve` (foreign trait from a generated server).
- The hand-written desugared `CompactTxStreamer` impl in `zaino-serve`
  (`zaino-state/src/rpc/grpc/service.rs`), which is tied to the generated trait.

These are foreign or generated traits the project does not own, so converting
them is neither possible by editing this tree nor part of getting *this
crate's* code off the macro.

## Consequences

- Trait-definition method signatures become longer than `async fn` sugar —
  `fn m(&self) -> impl SendFut<T>` versus `async fn m(&self) -> T`. The
  `SendFut` alias and ordinary `type` aliases for the larger result types keep
  this readable. Impl blocks are unaffected.
- The `async-trait` dependency declaration is removed from `zaino-state`'s
  `Cargo.toml` and from `[workspace.dependencies]` once the last direct use is
  gone. The crate continues to be compiled transitively for `tonic` and
  `jsonrpsee` until those upstreams migrate; no build-time or binary change is
  expected before then.
- RPITIT captures all in-scope generics and lifetimes by default, so
  `&self`-borrowing methods need no explicit lifetime bound — a minor
  simplification over what the longhand would otherwise require.
- The `Send` bound is now asserted per method through `SendFut`. Any new async
  trait method must return `impl SendFut<...>` (not a bare `impl Future<...>`)
  or it will fail to compile where the future crosses a spawn boundary; the
  alias makes the correct form the obvious one.
