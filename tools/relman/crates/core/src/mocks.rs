//! In-memory port implementations and fixtures for tests.
//!
//! Behind the `test-support` feature so it never ships in release builds.
//! Downstream crates enable it as a dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! relman-core = { workspace = true, features = ["test-support"] }
//! ```

mod clock;
pub mod fixtures;

pub use clock::FixedClock;
