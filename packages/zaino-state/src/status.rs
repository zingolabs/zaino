//! Re-export of the shared status vocabulary.
//!
//! [`Status`], [`StatusType`] and [`NamedAtomicStatus`] all live in
//! `zaino-common` so that a subsystem can report its status without depending
//! on `zaino-state`. This module exists only so the `crate::status::*` paths
//! already spread through this crate keep resolving.

pub use zaino_common::status::{NamedAtomicStatus, Status, StatusType};
