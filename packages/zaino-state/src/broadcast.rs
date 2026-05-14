//! Holds zaino-state::Broadcast, a thread-safe shared map paired with a
//! coalescible wakeup channel, used by the mempool and non-finalised state.
//!
//! Each [`Broadcast`] owns a [`DashMap<K, V>`] plus a
//! [`tokio::sync::watch`] sender carrying the producer's most recently
//! published [`StatusType`]. Calls to [`Broadcast::notify`] (and the
//! `insert*` / `remove` helpers that imply a notify) replace the
//! channel's current value and wake parked subscribers — they do **not**
//! enqueue per-call events. A subscriber that hasn't polled between two
//! sends sees only the second value; intermediate statuses are dropped.
//! Treat the channel as "wake up and re-read the map," not as a message
//! queue.

use dashmap::DashMap;
use std::{collections::HashSet, hash::Hash, sync::Arc};
use tokio::sync::watch;
use tracing::debug;

use crate::status::StatusType;

/// A thread-safe shared map paired with a coalescible wakeup channel.
///
/// Producers mutate the inner [`DashMap`] and call [`Broadcast::notify`]
/// (or one of the `insert*` / `remove` helpers that notifies inline) to
/// publish a [`StatusType`] on the watch sender. The watch channel only
/// retains the most recent value: if a subscriber hasn't polled between
/// two sends, the earlier value is lost. Subscribers receive wakeups,
/// not the full sequence of state transitions, and must read the map
/// directly on every wakeup.
#[derive(Clone)]
pub(crate) struct Broadcast<K, V> {
    state: Arc<DashMap<K, V>>,
    notifier: watch::Sender<StatusType>,
}

impl<K: Eq + Hash + Clone, V: Clone> Broadcast<K, V> {
    /// Creates a new [`Broadcast`], optionally exposes dashmap spec.
    pub(crate) fn new(capacity: Option<usize>, shard_amount: Option<usize>) -> Self {
        let (notifier, _) = watch::channel(StatusType::Spawning);
        let state = match (capacity, shard_amount) {
            (Some(capacity), Some(shard_amount)) => Arc::new(
                DashMap::with_capacity_and_shard_amount(capacity, shard_amount),
            ),
            (Some(capacity), None) => Arc::new(DashMap::with_capacity(capacity)),
            (None, Some(shard_amount)) => Arc::new(DashMap::with_shard_amount(shard_amount)),
            (None, None) => Arc::new(DashMap::new()),
        };

        Self { state, notifier }
    }

    /// Inserts or updates an entry. If `status` is `Some`, publishes it
    /// on the wakeup channel, replacing any pending value not yet
    /// observed by subscribers.
    #[allow(dead_code)]
    pub(crate) fn insert(&self, key: K, value: V, status: Option<StatusType>) {
        self.state.insert(key, value);
        if let Some(status) = status {
            let _ = self.notifier.send(status);
        }
    }

    /// Inserts or updates every `(key, value)` in `set`, then publishes
    /// `status` on the wakeup channel, replacing any pending value not
    /// yet observed by subscribers.
    #[allow(dead_code)]
    pub(crate) fn insert_set(&self, set: Vec<(K, V)>, status: StatusType) {
        for (key, value) in set {
            self.state.insert(key, value);
        }
        let _ = self.notifier.send(status);
    }

    /// Inserts only new entries from `set` (keys already present are
    /// left alone), then publishes `status` on the wakeup channel,
    /// replacing any pending value not yet observed by subscribers.
    pub(crate) fn insert_filtered_set(&self, set: Vec<(K, V)>, status: StatusType) {
        for (key, value) in set {
            // Check if the key is already in the map
            if self.state.get(&key).is_none() {
                self.state.insert(key, value);
            }
        }
        let _ = self.notifier.send(status);
    }

    /// Removes an entry. If `status` is `Some`, publishes it on the
    /// wakeup channel, replacing any pending value not yet observed by
    /// subscribers.
    #[allow(dead_code)]
    pub(crate) fn remove(&self, key: &K, status: Option<StatusType>) {
        self.state.remove(key);
        if let Some(status) = status {
            let _ = self.notifier.send(status);
        }
    }

    /// Retrieves a value from the state by key.
    #[allow(dead_code)]
    pub(crate) fn get(&self, key: &K) -> Option<Arc<V>> {
        self.state
            .get(key)
            .map(|entry| Arc::new((*entry.value()).clone()))
    }

    /// Retrieves a set of values from the state by a list of keys.
    #[allow(dead_code)]
    pub(crate) fn get_set(&self, keys: &[K]) -> Vec<(K, Arc<V>)> {
        keys.iter()
            .filter_map(|key| {
                self.state
                    .get(key)
                    .map(|entry| (key.clone(), Arc::new((*entry.value()).clone())))
            })
            .collect()
    }

    /// Checks if a key exists in the state.
    #[allow(dead_code)]
    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.state.contains_key(key)
    }

    /// Returns a [`watch::Receiver`] subscribed to the producer's most
    /// recent [`StatusType`]. The receiver observes coalescible wakeups,
    /// not a complete event stream: rapid sends collapse to the latest
    /// value from the receiver's perspective.
    pub(crate) fn subscribe(&self) -> watch::Receiver<StatusType> {
        self.notifier.subscribe()
    }

    /// Returns a [`BroadcastSubscriber`] to the [`Broadcast`].
    pub(crate) fn subscriber(&self) -> BroadcastSubscriber<K, V> {
        BroadcastSubscriber {
            state: self.get_state(),
            notifier: self.subscribe(),
        }
    }

    /// Provides read access to the internal state.
    pub(crate) fn get_state(&self) -> Arc<DashMap<K, V>> {
        Arc::clone(&self.state)
    }

    /// Returns the whole state excluding keys in the ignore list.
    #[allow(dead_code)]
    pub(crate) fn get_filtered_state(&self, ignore_list: &HashSet<K>) -> Vec<(K, V)> {
        self.state
            .iter()
            .filter(|entry| !ignore_list.contains(entry.key()))
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    /// Clears all entries from the state.
    pub(crate) fn clear(&self) {
        self.state.clear();
    }

    /// Returns the number of entries in the state.
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.state.len()
    }

    /// Returns true if the state is empty.
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// Publishes `status` on the wakeup channel, replacing any pending
    /// value not yet observed by subscribers. This is a coalescible
    /// wakeup, not a per-call event delivery — concurrent sends collapse
    /// to whichever value lands last. Logs at `debug` if no subscribers
    /// are currently connected.
    pub(crate) fn notify(&self, status: StatusType) {
        if self.notifier.send(status).is_err() {
            debug!("No subscribers are currently listening for updates.");
        }
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Default for Broadcast<K, V> {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl<K: Eq + Hash + Clone + std::fmt::Debug, V: Clone + std::fmt::Debug> std::fmt::Debug
    for Broadcast<K, V>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state_contents: Vec<_> = self
            .state
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        f.debug_struct("Broadcast")
            .field("state", &state_contents)
            .field("notifier", &"watch::Sender<StatusType>")
            .finish()
    }
}

/// Subscriber handle for a [`Broadcast`].
///
/// Holds a shared reference to the inner map and a [`watch::Receiver`]
/// that observes the producer's most recent [`StatusType`]. The receiver
/// delivers coalescible wakeups, not preserved events:
/// [`BroadcastSubscriber::wait_on_notifier`] returns once that latest
/// value changes, but may skip past intermediate values produced
/// between calls. Read the map directly for the current state on each
/// wakeup.
#[derive(Clone)]
pub(crate) struct BroadcastSubscriber<K, V> {
    state: Arc<DashMap<K, V>>,
    notifier: watch::Receiver<StatusType>,
}

impl<K: Eq + Hash + Clone, V: Clone> BroadcastSubscriber<K, V> {
    /// Awaits the next wakeup on the underlying [`watch::Receiver`] and
    /// returns the producer's *current* [`StatusType`]. If the producer
    /// published several values between calls, only the last one is
    /// observed — intermediate statuses are dropped by the watch
    /// channel. Callers that need to see every transition must use a
    /// different primitive.
    pub(crate) async fn wait_on_notifier(&mut self) -> Result<StatusType, watch::error::RecvError> {
        self.notifier.changed().await?;
        let status = *self.notifier.borrow();
        Ok(status)
    }

    /// Retrieves a value from the state by key.
    #[allow(dead_code)]
    pub(crate) fn get(&self, key: &K) -> Option<Arc<V>> {
        self.state
            .get(key)
            .map(|entry| Arc::new((*entry.value()).clone()))
    }

    /// Retrieves a set of values from the state by a list of keys.
    #[allow(dead_code)]
    pub(crate) fn get_set(&self, keys: &[K]) -> Vec<(K, Arc<V>)> {
        keys.iter()
            .filter_map(|key| {
                self.state
                    .get(key)
                    .map(|entry| (key.clone(), Arc::new((*entry.value()).clone())))
            })
            .collect()
    }

    /// Checks if a key exists in the state.
    #[allow(dead_code)]
    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.state.contains_key(key)
    }

    /// Returns the whole state excluding keys in the ignore list.
    pub(crate) fn get_filtered_state(&self, ignore_list: &HashSet<K>) -> Vec<(K, V)> {
        self.state
            .iter()
            .filter(|entry| !ignore_list.contains(entry.key()))
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    /// Returns the number of entries in the state.
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.state.len()
    }

    /// Returns true if the state is empty.
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.state.is_empty()
    }
}

impl<K: Eq + Hash + Clone + std::fmt::Debug, V: Clone + std::fmt::Debug> std::fmt::Debug
    for BroadcastSubscriber<K, V>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state_contents: Vec<_> = self
            .state
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        f.debug_struct("Broadcast")
            .field("state", &state_contents)
            .field("notifier", &"watch::Sender<StatusType>")
            .finish()
    }
}
