//! The production adapter must be able to serve every subsystem.
//!
//! Each capability alias states what one subsystem asks of the validator. If a
//! port trait is added to an alias but never implemented, or an adapter loses
//! an implementation, that is a compile error here rather than a failure when
//! the subsystem is finally wired up.
//!
//! These are assertions about types, so they do their work at compile time; the
//! test body exists only to instantiate them.

use zaino_state::source_caps::{
    ChainHeadSourceCaps, ChainIndexSourceCaps, FinalisedSourceCaps, MempoolSourceCaps,
};

fn assert_finalised<T: FinalisedSourceCaps>() {}
fn assert_chain_head<T: ChainHeadSourceCaps>() {}
fn assert_mempool<T: MempoolSourceCaps>() {}
fn assert_chain_index<T: ChainIndexSourceCaps>() {}

/// The adapter is the source every subsystem is wired to, so it has to
/// satisfy all of them.
#[test]
fn zebra_rpc_adapter_satisfies_every_subsystem() {
    assert_finalised::<zaino_source_zebra_rpc::ZebraRpcAdapter>();
    assert_chain_head::<zaino_source_zebra_rpc::ZebraRpcAdapter>();
    assert_mempool::<zaino_source_zebra_rpc::ZebraRpcAdapter>();
    assert_chain_index::<zaino_source_zebra_rpc::ZebraRpcAdapter>();
}
