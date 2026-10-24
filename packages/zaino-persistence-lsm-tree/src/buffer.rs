//! Commits not yet written, per namespace, in persistent (`imbl`) structures
//!
//! - clone = O(namespaces): every reader snapshot holds one
//! - one allocation per commit: every key, value and record a slice of it
//! - a key inserted and removed here = its value, then its tombstone in a newer segment (a
//!   removal is forgotten only when a merge cancels the pair)

use bytes::{Bytes, BytesMut};
use imbl::{OrdMap, Vector};
use zaino_persistence::{NamespaceKind, NamespaceSpec, WriteOp};
use zaino_primitives::types::BlockRef;

use crate::{engine::position, error::StoreError};

/// Heap per buffered map entry beyond its bytes: key + entry in a B-tree leaf at typical fill,
/// plus inner nodes (measured: `tests/buffer_heap.rs`, 36 → 8 B rows)
const MAP_ENTRY_HEAP: usize = 108;

/// Heap per buffered record beyond its bytes: its slice in an RRB chunk (measured likewise)
const RECORD_HEAP: usize = 32;

/// Heap per commit beyond its bytes: the shared allocation + its header (measured likewise)
const COMMIT_HEAP: usize = 48;

#[derive(Debug, Clone)]
pub(crate) struct Buffer {
    tip: Option<BlockRef>,
    held: Vec<Held>,
    item_bytes: usize,
    heap: usize,
}

/// One namespace's buffered data, declaration order
#[derive(Debug, Clone)]
enum Held {
    /// Records past durable, oldest first
    Sequence(Vector<Bytes>),
    /// Keys written since durable, key order
    Map(OrdMap<Bytes, Entry>),
}

/// A map row to write: `(key, value)`, `None` = tombstone
pub(crate) type Row<'a> = (&'a [u8], Option<&'a [u8]>);

/// A buffered key's state above durable
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    Value(Bytes),
    /// Removed here, its value durable: written as a tombstone (masks durable)
    Removed,
    /// Inserted (this value) then removed here: written as the value, then a newer tombstone
    Cancelled(Bytes),
}

impl Buffer {
    pub(crate) fn empty(specs: &[NamespaceSpec]) -> Self {
        let held = specs
            .iter()
            .map(|spec| match spec.kind {
                NamespaceKind::Sequence { .. } => Held::Sequence(Vector::new()),
                NamespaceKind::Map { .. } => Held::Map(OrdMap::new()),
            })
            .collect();
        Self {
            tip: None,
            held,
            item_bytes: 0,
            heap: 0,
        }
    }

    /// Last commit buffered (`None` = nothing buffered)
    pub(crate) fn tip(&self) -> Option<BlockRef> {
        self.tip
    }

    /// Key, value and record bytes held
    pub(crate) fn item_bytes(&self) -> usize {
        self.item_bytes
    }

    /// Heap held, estimated (the write threshold's measure)
    pub(crate) fn heap(&self) -> usize {
        self.heap
    }

    /// This buffer + `ops` at `tip`, sharing structure with `self`
    ///
    /// - `ops` passed `check_commit` first: a breach here is `Internal`, nothing changed
    pub(crate) fn with(
        &self,
        specs: &[NamespaceSpec],
        tip: BlockRef,
        ops: &[WriteOp],
    ) -> Result<Self, StoreError> {
        let mut bytes = BytesMut::with_capacity(ops.iter().fold(0, |sum, op| {
            let (key, value) = op_bytes(specs, op);
            sum.saturating_add(key.len()).saturating_add(value.len())
        }));
        for op in ops {
            let (key, value) = op_bytes(specs, op);
            bytes.extend_from_slice(key);
            bytes.extend_from_slice(value);
        }
        let bytes = bytes.freeze();

        let mut next = self.clone();
        let mut at = 0usize;
        let mut take = |len: usize| -> Result<Bytes, StoreError> {
            let end = at
                .checked_add(len)
                .filter(|end| *end <= bytes.len())
                .ok_or(StoreError::Internal("buffered bytes past the commit's"))?;
            let slice = bytes.slice(at..end);
            at = end;
            Ok(slice)
        };
        let (mut entries, mut records) = (0usize, 0usize);
        for op in ops {
            let (key, value) = op_bytes(specs, op);
            let (key, value) = (take(key.len())?, take(value.len())?);
            let namespace = match op {
                WriteOp::Put { namespace, .. } | WriteOp::Delete { namespace, .. } => *namespace,
            };
            let held = position(specs, namespace)
                .and_then(|index| next.held.get_mut(index))
                .ok_or(StoreError::Internal("an op of an undeclared namespace"))?;
            match (held, op) {
                (Held::Sequence(held), WriteOp::Put { .. }) => {
                    held.push_back(value);
                    records = records.saturating_add(1);
                }
                (Held::Map(held), WriteOp::Put { .. }) => {
                    held.insert(key, Entry::Value(value));
                    entries = entries.saturating_add(1);
                }
                (Held::Map(held), WriteOp::Delete { .. }) => {
                    let entry = match held.get(&key) {
                        Some(Entry::Value(value)) => Entry::Cancelled(value.clone()),
                        Some(Entry::Removed | Entry::Cancelled(_)) => {
                            return Err(StoreError::Internal("a buffered key removed twice"))
                        }
                        None => {
                            entries = entries.saturating_add(1);
                            Entry::Removed
                        }
                    };
                    held.insert(key, entry);
                }
                (Held::Sequence(_), WriteOp::Delete { .. }) => {
                    return Err(StoreError::Internal("a delete in a sequence"))
                }
            }
        }

        let grown = bytes
            .len()
            .saturating_add(COMMIT_HEAP)
            .saturating_add(entries.saturating_mul(MAP_ENTRY_HEAP))
            .saturating_add(records.saturating_mul(RECORD_HEAP));
        next.item_bytes = next.item_bytes.saturating_add(bytes.len());
        next.heap = next.heap.saturating_add(grown);
        next.tip = Some(tip);
        Ok(next)
    }

    /// Records of sequence `index` past durable (`None` = not a sequence)
    pub(crate) fn records(&self, index: usize) -> Option<&Vector<Bytes>> {
        match self.held.get(index)? {
            Held::Sequence(records) => Some(records),
            Held::Map(_) => None,
        }
    }

    /// Entries of map `index` (`None` = not a map)
    pub(crate) fn entries(&self, index: usize) -> Option<&OrdMap<Bytes, Entry>> {
        match self.held.get(index)? {
            Held::Map(entries) => Some(entries),
            Held::Sequence(_) => None,
        }
    }

    /// Map `index`'s rows to write, key order (`None` = tombstone; a cancelled pair = its value),
    /// and the cancelled keys, whose tombstones go in a newer segment
    pub(crate) fn map_rows(&self, index: usize) -> (Vec<Row<'_>>, Vec<&[u8]>) {
        let mut rows = Vec::new();
        let mut cancelled = Vec::new();
        for (key, entry) in self
            .entries(index)
            .into_iter()
            .flat_map(|entries| entries.iter())
        {
            match entry {
                Entry::Value(value) => rows.push((key.as_ref(), Some(value.as_ref()))),
                Entry::Removed => rows.push((key.as_ref(), None)),
                Entry::Cancelled(value) => {
                    rows.push((key.as_ref(), Some(value.as_ref())));
                    cancelled.push(key.as_ref());
                }
            }
        }
        (rows, cancelled)
    }
}

/// `(key, value)` bytes `op` buffers: a sequence keeps no key (its position), a removal no value
fn op_bytes<'a>(specs: &[NamespaceSpec], op: &'a WriteOp) -> (&'a [u8], &'a [u8]) {
    match op {
        WriteOp::Put {
            namespace,
            key,
            value,
        } => {
            let sequence = position(specs, *namespace)
                .and_then(|index| specs.get(index))
                .is_some_and(|spec| matches!(spec.kind, NamespaceKind::Sequence { .. }));
            match sequence {
                true => (&[], value),
                false => (key, value),
            }
        }
        WriteOp::Delete { key, .. } => (key, &[]),
    }
}

#[cfg(test)]
mod tests;
