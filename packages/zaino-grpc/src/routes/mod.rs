//! One module per route group, its handlers (registry + dispatch: [`crate::service`])

mod args;
pub(super) mod blocks;
pub(super) mod deprecated_nullifiers;
pub(super) mod info;
pub(super) mod mempool;
pub(super) mod transactions;
pub(super) mod transparent_address;
pub(super) mod tree_state;
