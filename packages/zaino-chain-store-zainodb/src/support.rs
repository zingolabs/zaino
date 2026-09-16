//! Small pieces the store depends on that have no better home yet.
//!
//! Each of these arrived with the finalised state and is either a one-line
//! convenience or a shape that will be replaced as the store moves fully onto
//! the domain ports. Grouped here so they are easy to find and easy to delete,
//! rather than scattered through the implementation.

use core::future::Future;

/// A future that can be sent across threads.
///
/// A trait alias in all but name. Present because the store's trait surface
/// returns futures from many methods, and writing the bound out at each one
/// obscures the signature that matters.
pub trait SendFut<T>: Future<Output = T> + Send {}
impl<T, F: Future<Output = T> + Send> SendFut<T> for F {}

/// Seconds since the Unix epoch, as a float.
///
/// For metric timestamps only. Returns zero rather than failing if the system
/// clock is before the epoch: a metric is not worth an error path, and a zero
/// reading is visibly wrong in a way a caller can act on.
#[cfg(feature = "prometheus")]
// Unused when `transparent_address_history_experimental` is on: that feature
// selects a different write path, and only the batched one records this gauge.
// The gap is in the experimental path, not here.
#[allow(dead_code)]
pub(crate) fn unix_now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
