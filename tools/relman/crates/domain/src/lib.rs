//! Domain: application services.
//!
//! Each service implements a driving port using only driven ports, held as
//! `Arc<dyn Trait>`. It never names a concrete adapter, so it is unit-tested
//! against the in-memory mocks from `relman-core`'s `test-support` feature.

pub mod render;
pub mod services;
