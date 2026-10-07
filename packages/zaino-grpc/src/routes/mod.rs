//! One module per route group, each answering its claimed paths (dispatch table:
//! [`crate::service`])

pub(super) mod blocks;
pub(super) mod chain;
pub(super) mod transparent_address;
pub(super) mod tree_state;
