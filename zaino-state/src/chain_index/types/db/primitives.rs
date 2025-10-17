//! Primitive database-serializable types.
//!
//! Contains basic primitive types that implement `ZainoVersionedSerialise`:
//! - Height
//! - ShardIndex
//! - ScriptType
//! - ShardRoot

mod height;

pub use height::{Height, GENESIS_HEIGHT};
