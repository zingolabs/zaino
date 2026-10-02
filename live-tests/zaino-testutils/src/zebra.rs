//! The zebrad the live suite stands up, as the `zebra!()` macro.
//!
//! `build.rs` resolves it from `[workspace.metadata.zaino.zebra]` and generates
//! the macro body; `zebra_spec` is that same resolution, compiled here under
//! `cfg(test)` so it carries unit tests.

include!(concat!(env!("OUT_DIR"), "/zebra_macro.rs"));

#[cfg(test)]
#[path = "../zebra_spec.rs"]
mod zebra_spec;
