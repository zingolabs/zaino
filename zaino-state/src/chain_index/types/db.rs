//! Database-serializable types for the chain index.
//!
//! This module contains all types that implement `ZainoVersionedSerde` and are used
//! for database persistence. These types follow strict versioning rules to maintain
//! backward compatibility across database schema changes.
//!
//! ## Rules for Types in This Module
//!
//! 1. **Never use external types as fields directly**
//!    - Store fundamental data in the struct
//!    - Implement `From`/`Into` or getters/setters for external type conversions
//!
//! 2. **Must implement ZainoVersionedSerde**
//!    - Follow stringent versioning rules outlined in the trait
//!    - Ensure backward compatibility
//!
//! 3. **Never change structs without proper migration**
//!    - Implement a new version when changes are needed
//!    - Update ZainoDB and implement necessary migrations

pub mod address;
pub mod block;
pub mod commitment;
pub mod legacy;
pub mod primitives;
pub mod shielded;
pub mod transaction;

pub use commitment::{CommitmentTreeData, CommitmentTreeRoots, CommitmentTreeSizes};
pub use legacy::*;
