//! The commit rules every backend enforces, checked once here.

use std::collections::{HashMap, HashSet};

use zaino_primitives::types::BlockRef;

use crate::backend::{
    sequence_position, Namespace, NamespaceKind, NamespaceSpec, Width, WriteOp, SEQUENCE_KEY_LEN,
};
use crate::error::{CommitError, Part, ReadError};

/// A map key's state before a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// Never inserted (or its removal is no longer recorded).
    Absent,
    /// Inserted and not removed.
    Present,
    /// Removed, and the backend still records the removal.
    Removed,
}

/// The state a commit is checked against, as the backend holds it now
/// (committed, buffered or durable alike).
pub trait CommitState {
    /// The last committed tip.
    fn tip(&self) -> Option<BlockRef>;

    /// Records a sequence namespace holds (= its next position).
    fn sequence_len(&self, namespace: Namespace) -> Result<u64, ReadError>;

    /// A map key's state.
    fn key_state(&self, namespace: Namespace, key: &[u8]) -> Result<KeyState, ReadError>;
}

/// Whether `ops` may be committed at `tip` over `state`: [`check_shape`], then
/// [`check_keys`].
pub fn check_commit(
    specs: &[NamespaceSpec],
    state: &impl CommitState,
    tip: BlockRef,
    ops: &[WriteOp],
) -> Result<(), CommitError> {
    check_shape(specs, state, tip, ops)?;
    check_keys(specs, state, ops)
}

/// Every rule that needs no map key's state (no disk reads): run first.
///
/// - `tip` above the last committed tip
/// - every namespace declared in `specs`
/// - sequence: `Put` at the next position, records of the declared width, no `Delete`
/// - map: declared widths, each key once per batch, `Delete` only with `deletes`
pub fn check_shape(
    specs: &[NamespaceSpec],
    state: &impl CommitState,
    tip: BlockRef,
    ops: &[WriteOp],
) -> Result<(), CommitError> {
    if let Some(last) = state.tip().filter(|last| tip.height <= last.height) {
        return Err(CommitError::TipNotAbove { tip, last });
    }
    let mut next: HashMap<Namespace, u64> = HashMap::new();
    let mut written: HashSet<(Namespace, &[u8])> = HashSet::new();
    for op in ops {
        let (namespace, key) = op_key(op);
        let name = || namespace.to_string();
        match (spec_of(specs, namespace)?.kind, op) {
            (NamespaceKind::Sequence { record }, WriteOp::Put { value, .. }) => {
                let expected = match next.get(&namespace) {
                    Some(&position) => position,
                    None => state
                        .sequence_len(namespace)
                        .map_err(CommitError::from_check)?,
                };
                let got = sequence_position(key);
                // u64::MAX holds no record: its successor would not fit
                let Some(after) = expected.checked_add(1).filter(|_| got == Some(expected)) else {
                    return Err(CommitError::NotNextPosition {
                        namespace: name(),
                        expected,
                        got,
                    });
                };
                check_width(namespace, Part::Record, record, value)?;
                next.insert(namespace, after);
            }
            (NamespaceKind::Sequence { .. }, WriteOp::Delete { .. }) => {
                return Err(CommitError::DeleteNotAllowed { namespace: name() });
            }
            (
                NamespaceKind::Map {
                    key: key_width,
                    value: value_width,
                    deletes,
                },
                op,
            ) => {
                check_width(namespace, Part::Key, Width::Fixed(key_width), key)?;
                if !written.insert((namespace, key)) {
                    return Err(CommitError::DuplicateInBatch { namespace: name() });
                }
                match op {
                    WriteOp::Put { value, .. } => {
                        check_width(namespace, Part::Value, Width::Fixed(value_width), value)?;
                    }
                    WriteOp::Delete { .. } if !deletes => {
                        return Err(CommitError::DeleteNotAllowed { namespace: name() });
                    }
                    WriteOp::Delete { .. } => {}
                }
            }
        }
    }
    Ok(())
}

/// Map keys against their state (after [`check_shape`]): `Put` of an absent key, `Delete` of
/// a present one; a removed key refused while the backend records the removal.
pub fn check_keys(
    specs: &[NamespaceSpec],
    state: &impl CommitState,
    ops: &[WriteOp],
) -> Result<(), CommitError> {
    for op in ops {
        let (namespace, key) = op_key(op);
        if !matches!(spec_of(specs, namespace)?.kind, NamespaceKind::Map { .. }) {
            continue;
        }
        let name = || namespace.to_string();
        let held = state
            .key_state(namespace, key)
            .map_err(CommitError::from_check)?;
        match (op, held) {
            (WriteOp::Put { .. }, KeyState::Absent)
            | (WriteOp::Delete { .. }, KeyState::Present) => {}
            (WriteOp::Put { .. }, KeyState::Present) => {
                return Err(CommitError::KeyExists { namespace: name() })
            }
            (WriteOp::Delete { .. }, KeyState::Absent) => {
                return Err(CommitError::KeyAbsent { namespace: name() })
            }
            (_, KeyState::Removed) => return Err(CommitError::KeyRemoved { namespace: name() }),
        }
    }
    Ok(())
}

/// `op`'s namespace and key
fn op_key(op: &WriteOp) -> (Namespace, &[u8]) {
    match op {
        WriteOp::Put { namespace, key, .. } | WriteOp::Delete { namespace, key } => {
            (*namespace, key.as_slice())
        }
    }
}

fn find_spec(specs: &[NamespaceSpec], namespace: Namespace) -> Option<&NamespaceSpec> {
    specs.iter().find(|spec| spec.namespace == namespace)
}

/// `namespace`'s spec, or `NamespaceNotFound`
fn spec_of(specs: &[NamespaceSpec], namespace: Namespace) -> Result<&NamespaceSpec, CommitError> {
    find_spec(specs, namespace).ok_or_else(|| CommitError::NamespaceNotFound(namespace.to_string()))
}

/// A point read's key: `namespace` declared in `specs`, `key` of its key width
/// (a sequence key is [`SEQUENCE_KEY_LEN`] bytes).
pub fn check_key(
    specs: &[NamespaceSpec],
    namespace: Namespace,
    key: &[u8],
) -> Result<(), ReadError> {
    let spec = find_spec(specs, namespace)
        .ok_or_else(|| ReadError::NamespaceNotFound(namespace.to_string()))?;
    let expected = match spec.kind {
        NamespaceKind::Sequence { .. } => SEQUENCE_KEY_LEN,
        NamespaceKind::Map { key, .. } => usize::try_from(key.get()).unwrap_or(usize::MAX),
    };
    match key.len() == expected {
        true => Ok(()),
        false => Err(ReadError::WidthMismatch {
            namespace: namespace.to_string(),
            expected,
            actual: key.len(),
        }),
    }
}

/// `bytes` of `width`, or `WidthMismatch`.
fn check_width(
    namespace: Namespace,
    part: Part,
    width: Width,
    bytes: &[u8],
) -> Result<(), CommitError> {
    match width {
        Width::Fixed(expected) if usize::try_from(expected.get()) != Ok(bytes.len()) => {
            Err(CommitError::WidthMismatch {
                namespace: namespace.to_string(),
                part,
                expected,
                actual: bytes.len(),
            })
        }
        Width::Fixed(_) | Width::Variable => Ok(()),
    }
}

/// Every namespace in `specs` declared once.
pub fn check_specs(specs: &[NamespaceSpec]) -> Result<(), crate::OpenError> {
    let mut seen = HashSet::new();
    match specs.iter().find(|spec| !seen.insert(spec.namespace)) {
        Some(spec) => Err(crate::OpenError::DuplicateNamespace(
            spec.namespace.to_string(),
        )),
        None => Ok(()),
    }
}
