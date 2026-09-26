//! Database-serializable types for the chain index.
//!
//! This module contains all types that implement `DbCodec` and are used
//! for database persistence.
//!
//! ## Rules for Types in This Module
//!
//! 1. **Never use external types as fields directly**
//!    - Store fundamental data in the struct
//!    - Implement `From`/`Into` or getters/setters for external type conversions
//!
//! 2. **Must implement `DbCodec`**
//!
//! 3. **Never change a struct's encoding without updating its golden and the schema hash golden**
//!    - Every existing database then rebuilds on its next start

pub mod address;
pub mod block;
pub mod commitment;
pub mod legacy;
pub mod metadata;
pub mod primitives;
pub mod shielded;
pub mod transaction;

pub use commitment::{CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes};
pub use legacy::*;
