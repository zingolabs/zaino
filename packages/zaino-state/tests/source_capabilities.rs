//! The composite must be able to serve every subsystem.
//!
//! Each capability alias states what one subsystem asks of the validator. If a
//! port trait is added to an alias but never implemented, or an adapter loses
//! an implementation, that is a compile error here rather than a failure when
//! the subsystem is finally wired up.
//!
//! These are assertions about types, so they do their work at compile time; the
//! test body exists only to instantiate them.
//!
//! One intended property is *not* asserted here: that `MempoolSourceCaps` is
//! unsatisfiable by the state-database adapter alone, because the mempool is
//! reachable only over JSON-RPC. Stable Rust has no negative trait bound, so
//! that cannot be written as a test — it is enforced by the readstate adapter
//! simply not implementing `GetMempoolTxids`, and recorded here so the absence
//! reads as deliberate.

use zaino_state::source_caps::{
    ChainHeadSourceCaps, ChainIndexSourceCaps, FinalisedSourceCaps, IndexerSourceCaps,
    MempoolSourceCaps,
};

fn assert_finalised<T: FinalisedSourceCaps>() {}
fn assert_chain_head<T: ChainHeadSourceCaps>() {}
fn assert_mempool<T: MempoolSourceCaps>() {}
fn assert_indexer<T: IndexerSourceCaps>() {}
fn assert_chain_index<T: ChainIndexSourceCaps>() {}

/// The composite is the source every subsystem is wired to, so it has to
/// satisfy all of them.
#[test]
fn zebra_validator_satisfies_every_subsystem() {
    assert_finalised::<zaino_source_zebra::ZebraValidator>();
    assert_chain_head::<zaino_source_zebra::ZebraValidator>();
    assert_mempool::<zaino_source_zebra::ZebraValidator>();
    assert_indexer::<zaino_source_zebra::ZebraValidator>();
    assert_chain_index::<zaino_source_zebra::ZebraValidator>();
}
