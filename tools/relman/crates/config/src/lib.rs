//! Config adapter: parse the repo-committed `relman.toml`.
//!
//! `relman.toml` is the authority for relman's governed versioning targets and
//! options. This crate deserializes it (serde + toml) into raw structs, then
//! converts into core newtypes at the composition-root boundary — applying
//! defaults and parse-don't-validate so invalid input fails at [`load`], not
//! deep in a later slice. The typed result is [`ReleaseConfig`].

mod config;
mod error;
mod load;

pub use config::ReleaseConfig;
pub use error::ConfigError;
pub use load::load;
