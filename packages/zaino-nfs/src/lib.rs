#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod core;
mod fetch;
mod graph;

pub use crate::core::{Diverged, Final, Input, NfsCore, Output, SnapshotTip};
pub use crate::fetch::{check_block, Answer, Checked, Misanswer};

/// Panic message of `run` (`None` = it returned): fire drills
#[cfg(test)]
fn fired(run: impl FnOnce()) -> Option<String> {
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).err()?;
    let message = panic.downcast_ref::<String>().cloned();
    message.or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
}
