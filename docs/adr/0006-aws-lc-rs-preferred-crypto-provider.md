# aws-lc-rs is the preferred CryptoProvider; classical key exchange is deprecating

## Status

accepted (reverses the ring choice made on zingolabs/zaino#1366 before it
merged)

## Context and decision

Zaino installs a rustls `CryptoProvider` as the process-wide default at
its two TLS boundaries (`zaino-serve`'s gRPC server, `zaino-fetch`'s
jsonrpsee connector) via `zaino_common::crypto::ensure_default_crypto_provider`
(zingolabs/zaino#1360). The provider was initially pinned to **ring** for
two reasons: zebra used ring (shared attack surface across the deployed
stack), and ring avoided the `aws-lc-sys` + `cmake` build dependencies.

Daira Emma argued for **aws-lc-rs** instead: post-quantum TLS key
exchange in the rustls ecosystem requires the aws-lc-rs provider — the
ring provider has no ML-KEM support at all — and the provider
architecture makes the swap cheap
(<https://crates.io/crates/rustls-post-quantum>,
<https://www.reddit.com/r/rust/comments/1de13y6/comment/l89pbmc/>).

We decide:

1. **aws-lc-rs is zaino's preferred CryptoProvider** — a *preference,
   not a mandate*: installation stays first-install-wins, so an embedder
   (e.g. zallet) that installs a provider before zaino keeps its choice.
   Enforcement lives in the feature graph (`rustls` `aws_lc_rs` feature,
   tonic `tls-aws-lc`), not at runtime.
2. **rustls's `prefer-post-quantum` feature is enabled**, reordering the
   provider's default key-exchange groups so the hybrid X25519MLKEM768
   leads (same membership; in rustls 0.23 the hybrid is in the default
   set either way). Verified effect in rustls 0.23.41: on zaino's
   *outbound* (client-role) TLS the hybrid share is offered upfront, and
   rustls adds the hybrid's X25519 component as a free second key share,
   so classical-only servers negotiate without a HelloRetryRequest — the
   cost is ~1.2 KB of ClientHello, not a round trip. On the *server*
   side rustls follows the client's group preference, so inbound
   negotiation outcomes are unchanged by the feature; hybrid uptake on
   inbound connections is entirely client-driven, which is why the
   deprecation below is gated on client stacks.
3. **Classical key exchange (X25519, SECP256R1, SECP384R1) remains
   accepted but is deprecated.** Refusing it today would break both
   flagship wallets: zingolib's TLS stack is feature-pinned to ring (no
   ML-KEM possible), and ZODL rides mobile platform TLS where hybrid
   support is still rolling out. The refusal precondition — zingolib on
   aws-lc-rs with hybrid preferred, and ZODL's Android/iOS platform
   stacks negotiating X25519MLKEM768 — is tracked in a dedicated issue;
   deprecation is documented in the changelog rather than signalled at
   runtime.
4. **Ring stays in `Cargo.lock`, tolerated and fenced, until zebra drops
   it.** `zebra-rpc` hard-enables `zebra-node-services/rpc-client`,
   whose reqwest 0.12 `rustls-tls` feature compiles ring in — reqwest
   0.12 offers no aws-lc alternative, only `*-no-provider` variants. This
   is dormant code, not a live handshake path: reqwest consults the
   process-default provider first and falls back to ring only when none
   is installed, so both zaino (which installs aws-lc-rs early) and
   embedders (whose earlier install wins) preempt it. A CI lint pins
   `cargo tree --invert ring` to exactly this one known path and fails on
   drift in *either* direction: a new ring consumer is a regression to
   investigate; the path vanishing means zebra dropped ring, the
   allowlist gets deleted, and the guard tightens to "ring absent".

## Considered options

- **Stay on ring, aligned with zebra.** Rejected: forecloses post-quantum
  key exchange entirely; the harvest-now-decrypt-later argument applies
  to wallet↔zaino TLS traffic today.
- **Refuse classical key exchange (hybrid-only).** Rejected for now:
  hard-locks out every wallet whose TLS stack lacks X25519MLKEM768,
  which today is all of them. Revisit when the tracked precondition is
  met.
- **Fork-pin zebra to purge ring from the lock immediately.** Rejected:
  the fork tax (pin churn here and in the wallet-tests workspace) buys
  only the removal of dormant code; downstream is already protected by
  provider preemption. Instead we upstream the fix to zebra —
  `zebra-node-services` switches reqwest to a `*-no-provider` feature
  and zebrad installs its own default provider (the same pattern as
  zaino's #1360 fix) — and pick it up at the next zebra bump.

## Consequences

- `aws-lc-sys` and `cmake` return to the build graph (both build images
  already install cmake).
- Zaino diverges from zebra's provider until zebra moves; the CI lint
  makes the eventual convergence loud instead of silent.
- TLS 1.2 remains available only as long as classical key exchange
  does; the refusal that ends the classical deprecation also implies
  TLS 1.3-only.
