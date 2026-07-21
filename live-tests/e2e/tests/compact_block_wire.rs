//! Wire-tier (zainod gRPC) era tests for compact-block serving.
//!
//! The dev version's e2e-exclusive predicate was **wire fidelity**: the compact blocks a
//! real tonic client receives from a running zainod equal, block for block, what the
//! in-process subscriber produces for the same request — the only tier crossing the
//! protobuf encode → network → decode boundary and zainod's gRPC server.
//!
//! Under ztest that comparison is structurally gone, in a good way: live-tests links no
//! zaino-state code, so there IS no in-process subscriber to differ from. *Every*
//! `get_block_range` call in the suite already streams over zainod's real gRPC
//! `CompactTxStreamer` (`ZainoIndexer` opens the tonic channel internally), so the wire
//! round-trip is exercised by every block-range test — e.g. `block_range_returns_*` in
//! `wallet_to_validator.rs` and the sapling/orchard tree-size + oracle-parity walk in
//! `clientless/tests/compact_block_consistency.rs`. The "wire == in-process" invariant is
//! now the identity, so a dedicated wire-fidelity test would be vacuous.
//!
//! The served stream's **NU6.3 / Ironwood era composition** IS now expressible: ztest
//! has an `NetworkUpgrade::Nu6_3` variant and its `compact_formats` proto carries
//! `ironwood_actions`. That coverage — the orchard→ironwood coinbase-routing flip and the
//! ironwood tree-size delta, over the same real zainod gRPC stream — is carried by
//! `clientless/tests/compact_block_consistency.rs::compact_blocks_carry_ironwood_after_nu6_3_zebrad`.
//! A separate e2e wire test would only duplicate it, so this file keeps a single stub
//! marking the gap rather than a second copy.

/// COVERAGE NOTE — NU6.3 / Ironwood era composition lives in `clientless` (issue #1368).
///
/// The ironwood era-composition assertions are ported and run over zainod's real gRPC
/// stream in `clientless`'s `compact_blocks_carry_ironwood_after_nu6_3_zebrad` (ztest now
/// models NU6.3 and decodes `ironwood_actions`). Both that test and any e2e wire variant
/// are blocked on the same remaining dependency — an NU6.3-capable validator image wired
/// into ztest (`ZEBRAD_NU6_3_RELEASE`) — and the e2e variant would add no coverage the
/// clientless one lacks, so it is intentionally not duplicated here.
/// See <https://github.com/zingolabs/zaino/issues/1368>.
#[ignore = "covered by clientless::compact_blocks_carry_ironwood_after_nu6_3_zebrad; needs an NU6.3 image — see #1368"]
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_era_wire_serving_zebrad() {
    panic!(
        "not a gap in ztest: NU6.3 era composition over the wire is covered by \
         clientless::compact_blocks_carry_ironwood_after_nu6_3_zebrad. This e2e variant is \
         intentionally left un-implemented to avoid duplication; it would need the same \
         NU6.3-capable validator image. See https://github.com/zingolabs/zaino/issues/1368."
    );
}
