//! Core: the center of the hexagon.
//!
//! Pure types and the port traits that define the application's seams.
//! Depends on nothing but small utility crates — no adapters, no I/O.
//!
//! - [`types`] — value types. Newtypes enforce their invariants at
//!   construction (parse, don't validate).
//! - [`ports`] — the seams: **driven** (outbound) traits that adapters
//!   implement, and **driving** (inbound) traits that the domain implements
//!   and callers consume.
//! - [`mocks`] — in-memory port implementations for tests, behind the
//!   `test-support` feature.

pub mod ports;
pub mod types;

#[cfg(feature = "test-support")]
pub mod mocks;
