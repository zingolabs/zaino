//! Measure one persistence backend in isolation, so two implementations can be
//! swapped in place and compared.
//!
//! This is not the pipeline benchmark. `bench/sync-bench` measures a validator
//! through the provisioner and engine into a store, at cluster scale; it answers
//! "how fast does zaino sync". This crate removes everything but the
//! [`zaino_persistence`] seam and answers a narrower question: **given the same
//! write and read instructions, what does this backend cost?**
//!
//! Every workload is generic over [`target::BenchTarget`], and the backends are
//! criterion *inputs* rather than a compile-time choice, so one `cargo bench`
//! reports them side by side and a new backend joins the comparison by
//! implementing one trait and registering one line. See `usage.md` for the run
//! and baseline recipe.
//!
//! # What makes the numbers honest
//!
//! - **Real bytes.** Records are encoded through the production codecs
//!   ([`data`]), so key and value sizes are the indexer's, not a guess.
//! - **Real batching.** A write commits every index's ops for a batch of blocks
//!   plus the watermark in one atomic call, and flushes at the end, as the sync
//!   engine does ([`workloads::commit_batches`]).
//! - **Real read shapes.** A served compact block is a seven-namespace fan-out
//!   at one height, which is what [`workloads::compose_block`] does.
//! - **Deterministic input.** A seeded generator, so both backends see the same
//!   bytes and a re-run sees them again.
//! - **A correctness gate.** [`workloads::Populated::verify`] reads every header
//!   entry back and compares it, so a backend cannot win by losing data.
#![forbid(unsafe_code)]

pub mod data;
pub mod error;
pub mod target;
pub mod workloads;

/// How much work a run does, and the seed its records come from.
///
/// The defaults are sized to finish in seconds on a laptop, which is smaller
/// than production: the engine commits 1000-block batches, and a run that wants
/// that shape sets it. Two runs compared against each other must use the same
/// scale, so the scale appears in every benchmark's name.
#[derive(Debug, Clone, Copy)]
pub struct Scale {
    /// Blocks generated and written.
    pub blocks: u32,
    /// Blocks per atomic commit.
    pub batch_size: u32,
    /// Seed for the record generator.
    pub seed: u64,
    /// How much material each block carries.
    pub shape: data::Shape,
}

impl Default for Scale {
    fn default() -> Self {
        Self {
            blocks: 256,
            batch_size: 64,
            seed: 0x5EED,
            shape: data::Shape::Typical,
        }
    }
}

impl Scale {
    /// The scale for this run: the defaults, overridden by environment.
    ///
    /// Read once at bench setup. The knobs are `BACKEND_BENCH_BLOCKS`,
    /// `BACKEND_BENCH_BATCH`, `BACKEND_BENCH_SEED` and `BACKEND_BENCH_SHAPE`
    /// (`light`, `typical` or `sandblast`); an unparseable value keeps the
    /// default rather than failing a run, and every value lands in the
    /// benchmark's name so a mismatch cannot hide inside a baseline comparison.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            blocks: env_parsed("BACKEND_BENCH_BLOCKS").unwrap_or(default.blocks),
            batch_size: env_parsed("BACKEND_BENCH_BATCH").unwrap_or(default.batch_size),
            seed: env_parsed("BACKEND_BENCH_SEED").unwrap_or(default.seed),
            shape: match std::env::var("BACKEND_BENCH_SHAPE").as_deref() {
                Ok("light") => data::Shape::Light,
                Ok("sandblast") => data::Shape::Sandblast,
                Ok("typical") | Ok(_) | Err(_) => default.shape,
            },
        }
    }

    /// The label every benchmark name carries, so a report says what it measured
    /// and criterion keys its baselines per scale rather than across scales.
    pub fn label(&self) -> String {
        format!(
            "{}-{}b-{}",
            self.shape.as_str(),
            self.blocks,
            self.batch_size
        )
    }
}

/// A `u32`/`u64` knob read from the environment, or `None` when unset or
/// unparseable.
fn env_parsed<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_names_the_shape_and_the_scale() {
        let scale = Scale {
            blocks: 128,
            batch_size: 32,
            seed: 1,
            shape: data::Shape::Sandblast,
        };

        assert_eq!(scale.label(), "sandblast-128b-32");
    }

    /// An unset environment must leave the defaults intact, so a plain
    /// `cargo bench` is reproducible.
    #[test]
    fn an_unset_environment_yields_the_defaults() {
        // The knobs are read by name; none is set in the test process.
        let scale = Scale::from_env();
        let default = Scale::default();

        assert_eq!(
            (scale.blocks, scale.batch_size, scale.seed),
            (default.blocks, default.batch_size, default.seed)
        );
    }
}
