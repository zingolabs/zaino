# `gettxoutsetinfo` behind a non-default feature gate

## Status

proposed

## Context and decision

`gettxoutsetinfo` is the sole consumer of the finalised **txout-set accumulator**
(schema table #9, `tx_out_set_info_accumulator`). The accumulator's from-genesis
build holds the whole spent-set in memory and is the OOM-prone, validator-loading
step that also makes every EXP-0001 rebuild-and-cutover expensive (see
`docs/notes/txout-set-accumulator.md`). The RPC is additionally **not implemented
by zebra** — the accumulator exists precisely because the validator cannot serve
`gettxoutsetinfo` cheaply.

Our key customer does not need `gettxoutsetinfo`. We put the whole capability —
the accumulator table, its build, its write-path maintenance, and the RPC's
ability to compute a result — behind a **non-default Cargo feature**,
`gettxoutsetinfo`, declared in `zaino-state` and re-exported through `zaino-serve`
and `zainod`. The default build does **not** include it.

The gate mirrors the existing `transparent_address_history_experimental` pattern:
the accumulator stays *described* in `db_schema_v1.txt` (so the schema hash is
unchanged and the gate has **zero** interaction with EXP-0001 rebuild-and-cutover —
no schema fork, no refuse/rebuild), and its physical presence is governed by the
feature plus the accumulator's existing watermark. With the feature off the table
is never created and no build ever runs; with it on, an accumulator-absent DB has
the index built lazily.

The cut is made at the **capability-dispatch seam**: the cost path (field, table
creation, builders, write-path maintenance, migration Stage C) is hard-`#[cfg]`'d
out, while method *signatures* stay stable and the dispatch body returns
`FinalisedStateError::FeatureUnavailable` when the feature is off — the same
mechanism the V0/Ephemeral backends already use to express an absent capability.
Layers above the seam (the NFS fold in `chain_index`, both backends, the indexer
trait, the JSON-RPC handler) are unchanged; the typed error propagates and the
handler returns "unsupported in this build" rather than being compiled away.

## Considered options (rejected)

- **Default-on / opt-out** (`default = ["gettxoutsetinfo"]`, customer builds
  `--no-default-features`). Protects existing users — only the customer's build
  drops the RPC. Rejected: we want the *default* binary to be the lean, safe,
  zebra-compatible one, with the accumulator's cost paid only by deployments that
  explicitly ask for it. (Contrast `zcashd_support`, which is default-on because
  the goal there is graceful deprecation, not making the lean build the default.)
- **Validator passthrough when the feature is off.** A client method
  (`JsonRpSeeConnector::get_tx_out_set_info`) already exists. Rejected: it is
  **zcashd-only** (zebra does not implement the RPC), so it would silently break
  under the strategic zebra direction; it dumps an unbounded full-UTXO scan on
  the validator; and it re-adds the passthrough EXP-0001 rejected for capabilities.
- **Compile the RPC method out of the JSON-RPC trait.** Rejected: feature-varying
  trait surfaces complicate client codegen and diverge from the house style (the
  `address_history` gate touches zero RPC/indexer/backend cfg points).
- **Gate the whole vertical** (field → reader → capability trait → NFS fold →
  indexer trait → backends). Rejected: feature-varying trait surfaces and
  object-safety/bound churn across both backends, for no gain over gating at the
  capability seam.
- **Edit `db_schema_v1.txt` to drop table #9.** Rejected: the one-time schema-hash
  change would make every existing deployed DB a hash mismatch → "older" → a
  forced from-genesis EXP-0001 rebuild on upgrade, for bookkeeping only.

## Consequences

- **Breaking: the default build no longer serves `gettxoutsetinfo`.** It shipped
  on-by-default in 0.4.0; the default next-release binary returns a typed
  "unsupported in this build" error. Restoring it requires
  `--features gettxoutsetinfo`, which also re-enables the accumulator and its
  from-genesis build cost. Documented in the per-crate CHANGELOGs. It is a niche
  full-UTXO-set-stats RPC, rarely used by wallet clients, which makes the trade
  acceptable.
- **No EXP-0001 interaction.** Because the schema hash is feature-independent,
  feature-on and feature-off DBs are hash-compatible; neither build refuses the
  other and toggling the feature never triggers a *schema* rebuild — only, when
  turning it on against an accumulator-absent DB, the accumulator index build.
- **CI must build and test both feature states.** Without the off-state job the
  gate bit-rots: the feature-off compile (which removes the cost path) and the
  "RPC returns FeatureUnavailable / no accumulator table created" behavior go
  unverified. Accumulator tests move under `#[cfg(feature = "gettxoutsetinfo")]`,
  plus one off-state test asserting the typed error.
- **The accumulator OOM/rebuild analysis becomes opt-in.** For the default build,
  the bug-C cost path in EXP-0001 simply does not exist.
