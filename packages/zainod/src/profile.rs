//! Zero-privilege CPU profiling for Zaino (the ztest profiling contract,
//! `docs/how-to-profile.md`).
//!
//! Linked only under `--features profile`. A single such image runs unprofiled
//! when `ZTEST_PROFILE` is unset — no [`pprof::ProfilerGuard`] is created, so
//! there is zero overhead — and profiled when it is set. The guard spans the
//! whole process lifetime; the report is built on a normal thread from the
//! graceful-shutdown path (never inside a signal handler — pprof report-building
//! is not async-signal-safe), so the flamegraph covers the entire run.

use std::env;

use tracing::{info, warn};

/// Default sampling frequency (Hz). A sync test runs 10–600 min, so the sample
/// rate is the lever on profiler overhead and the *primary* lever on artifact
/// size. 100 Hz keeps overhead ~1% over a multi-hour run while still resolving
/// the hot Rust paths (pprof's own default is 99). Override with
/// `ZTEST_PROFILE_HZ` for a longer/shorter run.
///
/// It is not the only lever on size: see [`write_profile`], where gzip takes a
/// measured 4x off the artifact for free. Sampling less to save bytes trades
/// away resolution that cannot be recovered; compressing trades away nothing.
const DEFAULT_FREQUENCY_HZ: i32 = 100;

/// Sampling frequency: `ZTEST_PROFILE_HZ` if a positive integer, else the default.
fn frequency_hz() -> i32 {
    env::var("ZTEST_PROFILE_HZ")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|&hz| hz > 0)
        .unwrap_or(DEFAULT_FREQUENCY_HZ)
}

/// Open a process-wide profiler when `ZTEST_PROFILE` is set, else `None`.
///
/// The `blocklist` is mandatory: `SIGPROF`-driven stack unwinding is not safe
/// through these frames and omitting it risks a deadlock. The cost is that
/// native (LMDB C) frames appear as opaque leaves — the graph is Rust-level.
pub fn start_profiler() -> Option<pprof::ProfilerGuard<'static>> {
    if env::var_os("ZTEST_PROFILE").is_none() {
        return None;
    }
    let hz = frequency_hz();
    match pprof::ProfilerGuardBuilder::default()
        .frequency(hz)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
    {
        Ok(guard) => {
            info!(hz, "ZTEST_PROFILE set: CPU profiler started");
            Some(guard)
        }
        Err(e) => {
            warn!(%e, "failed to start CPU profiler; continuing unprofiled");
            None
        }
    }
}

/// Build the report and write `profile.pb` (the gzipped pprof protobuf) into
/// `ZTEST_PROFILE_OUT` (default the cwd).
///
/// Only the `.pb` is written: it is the source-of-truth artifact — string-interned
/// and an order of magnitude smaller than a rendered SVG — and every consumer
/// (speedscope.app, pprof.me, `go tool pprof`) renders the flamegraph from it on
/// demand, interactively and diffably. A flat SVG pre-render would be larger and
/// strictly less useful.
///
/// Consumes the guard so sampling has stopped before the report is built. Never
/// call this from a signal handler.
pub fn write_profile(guard: pprof::ProfilerGuard<'_>) {
    let report = match guard.report().build() {
        Ok(r) => r,
        Err(e) => {
            warn!(%e, "failed to build CPU profile report");
            return;
        }
    };
    let dir = env::var("ZTEST_PROFILE_OUT").unwrap_or_else(|_| ".".into());

    match report.pprof() {
        // `protobuf-codec` gives a rust-protobuf message; `write_to_bytes` is
        // its serializer (the `encode(&mut buf)` in some docs is the prost API,
        // which this feature does not use).
        Ok(profile) => {
            use pprof::protos::Message;
            match profile.write_to_bytes() {
                Ok(buf) => write_gzipped(&dir, &buf),
                Err(e) => warn!(%e, "failed to serialize pprof protobuf"),
            }
        }
        Err(e) => warn!(%e, "failed to build pprof protobuf"),
    }
}

/// Write `profile.pb`, gzipped.
///
/// The extension stays `.pb`: gzip is the pprof format's conventional encoding
/// rather than a separate container, and every consumer (`go tool pprof`,
/// flameshow, speedscope.app, pprof.me) sniffs for the magic bytes. Renaming to
/// `.pb.gz` would break `ztest sync perf`'s artifact discovery for no gain.
///
/// Worth doing because the saving is large and free. A pprof profile interns
/// every symbol once and then repeats the same location ids on every sample —
/// notably the ~14-frame thread/tokio prologue that opens every stack — which is
/// close to the ideal case for LZ77. Measured on a 9m21s `zaino-state-sync`
/// profile: **156,399 → 39,699 bytes**, a 3.9x reduction with no loss of a
/// single sample or frame.
///
/// This is the right layer for the saving. Dropping the boilerplate frames at
/// collection time (via pprof-rs's `ReportBuilder::frames_post_processor`) was
/// measured at 615 bytes, 1.7%, and would destroy the information
/// irreversibly — `ztest sync perf --raw` could no longer show the runtime
/// overhead, because it would never have been recorded. Compression gets 3.9x
/// and keeps the profile faithful, so the elision stays a *view* concern.
///
/// [`Compression::best`] because this runs once, on an already-terminating
/// process, where a few hundred milliseconds cost nothing and the artifact is
/// about to cross a `kubectl cp` from a cluster.
fn write_gzipped(dir: &str, buf: &[u8]) {
    let compressed = match gzip(buf) {
        Ok(bytes) => bytes,
        // The uncompressed profile is still a valid pprof artifact, so a
        // compressor failure must cost bytes on the wire, never the profile
        // itself — this runs at shutdown, after the run it describes is over
        // and cannot be repeated.
        Err(e) => {
            warn!(%e, "failed to gzip profile; writing it uncompressed");
            buf.to_vec()
        }
    };
    match std::fs::write(format!("{dir}/profile.pb"), &compressed) {
        Ok(()) => info!(
            %dir,
            bytes = compressed.len(),
            uncompressed = buf.len(),
            "wrote profile.pb"
        ),
        Err(e) => warn!(%e, "failed to write profile.pb"),
    }
}

/// gzip `buf` at maximum compression.
///
/// Separated from the write so the encoding is exercised by a test without
/// needing a running profiler or a writable directory.
fn gzip(buf: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write as _;

    use flate2::write::GzEncoder;
    use flate2::Compression;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(buf)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pprof profile is bytes on a wire between a cluster pod and a laptop,
    /// so the two properties that matter are that consumers can still recognise
    /// it and that nothing was lost. The magic number is what every viewer
    /// sniffs for — without it `.pb` would be an unreadable file, not a smaller
    /// one.
    #[test]
    fn the_profile_is_gzip_framed_and_lossless() {
        use std::io::Read as _;

        // Deliberately repetitive: a pprof profile is one interned symbol table
        // plus the same location ids repeated across every sample, which is the
        // shape the 3.9x measurement comes from.
        let profile: Vec<u8> = (0..4096u32)
            .flat_map(|i| [1, 2, 3, (i % 7) as u8])
            .collect();
        let compressed = gzip(&profile).expect("in-memory gzip cannot fail");

        assert_eq!(&compressed[..2], &[0x1f, 0x8b], "gzip magic number");
        assert!(
            compressed.len() < profile.len(),
            "{} -> {}",
            profile.len(),
            compressed.len()
        );

        let mut round_tripped = Vec::new();
        flate2::read::GzDecoder::new(&compressed[..])
            .read_to_end(&mut round_tripped)
            .expect("decode what we just encoded");
        assert_eq!(round_tripped, profile, "compression must be lossless");
    }

    /// The degenerate input must not panic or produce something a decoder
    /// rejects: `report.pprof()` on a run that recorded no samples is a valid,
    /// nearly-empty profile, and shutdown is the worst possible place to abort.
    #[test]
    fn an_empty_profile_still_produces_a_valid_gzip_stream() {
        let compressed = gzip(&[]).expect("in-memory gzip cannot fail");
        assert_eq!(&compressed[..2], &[0x1f, 0x8b]);
    }
}
