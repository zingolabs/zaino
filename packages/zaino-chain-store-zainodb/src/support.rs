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
