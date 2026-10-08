//! Shared fixtures, oracles, and assertions for the conformance suite.
//!
//! The suite checks a backend against an independent *oracle* — a sorted map
//! built by replaying the same [`WriteOp`]s in Rust — rather than against a
//! second backend. One backend per property keeps the factory contract simple
//! (no two live handles at once) and makes the oracle a genuinely independent
//! reference for "a direct build of the same commits".

use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;

use crate::backend::{
    Backend, BackendReader, KeyOrder, Namespace, NamespaceSpec, RawKey, RawValue, WriteOp,
};

/// A walk-ordered namespace: height-led keys that arrive strictly ascending and
/// are never overwritten or deleted, so an append-only backend can accept them.
pub(crate) const WALK_NS: Namespace = Namespace::new("headers");
/// A scattered namespace: hash-led keys that arrive out of byte order.
pub(crate) const SCATTERED_NS: Namespace = Namespace::new("address_history");
/// A second scattered namespace, to exercise multi-namespace isolation and the
/// deferral of more than one namespace at once.
pub(crate) const SCATTERED_NS_2: Namespace = Namespace::new("txid_location");
/// A reserved meta namespace: engine bookkeeping written directly (and freely
/// overwritten), never deferred, never appended.
pub(crate) const META_NS: Namespace = Namespace::new("_watermark");

/// Four scattered keys whose bytewise (lexicographic) order differs *both* from
/// the order they are inserted in *and* from their little-endian numeric order —
/// so an ordering assertion against them is discriminating, not satisfied by
/// accident. Bytewise ascending is `[00 02], [00 10], [01 01], [02 00]`.
pub(crate) const SCATTERED_KEYS: [[u8; 2]; 4] = [
    [0x02, 0x00], // inserted 1st; LE u16 = 0x0002; bytewise last
    [0x00, 0x02], // inserted 2nd; LE u16 = 0x0200; bytewise first
    [0x01, 0x01], // inserted 3rd; LE u16 = 0x0101; bytewise third
    [0x00, 0x10], // inserted 4th; LE u16 = 0x1000; bytewise second
];

/// The canonical namespace set a conformance backend is opened with: one of each
/// [`KeyOrder`], plus a second scattered namespace.
pub(crate) fn specs() -> Vec<NamespaceSpec> {
    vec![
        NamespaceSpec {
            namespace: WALK_NS,
            key_order: KeyOrder::WalkOrdered,
        },
        NamespaceSpec {
            namespace: SCATTERED_NS,
            key_order: KeyOrder::Scattered,
        },
        NamespaceSpec {
            namespace: SCATTERED_NS_2,
            key_order: KeyOrder::Scattered,
        },
        NamespaceSpec::meta(META_NS),
    ]
}

/// A big-endian height key, as walk-ordered indexes produce.
pub(crate) fn height_key(height: u32) -> RawKey {
    height.to_be_bytes().to_vec()
}

/// Build a [`WriteOp::Put`].
pub(crate) fn put(namespace: Namespace, key: RawKey, value: RawValue) -> WriteOp {
    WriteOp::Put {
        namespace,
        key,
        value,
    }
}

/// Build a [`WriteOp::Delete`].
pub(crate) fn delete(namespace: Namespace, key: RawKey) -> WriteOp {
    WriteOp::Delete { namespace, key }
}

/// A commit sequence for the non-bulk properties. Walk keys are strictly
/// ascending; scattered keys arrive out of byte order and include an overwrite
/// and a delete; the meta watermark is overwritten each batch. Rebuilt on each
/// call because [`WriteOp`] is not `Clone`.
pub(crate) fn direct_batches() -> Vec<Vec<WriteOp>> {
    vec![
        vec![
            put(WALK_NS, height_key(0), b"h0".to_vec()),
            put(WALK_NS, height_key(1), b"h1".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[0].to_vec(), b"a0".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[1].to_vec(), b"a1".to_vec()),
            put(SCATTERED_NS_2, SCATTERED_KEYS[2].to_vec(), b"t-c".to_vec()),
            put(META_NS, b"watermark".to_vec(), 1u64.to_be_bytes().to_vec()),
        ],
        vec![
            put(WALK_NS, height_key(2), b"h2".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[2].to_vec(), b"a2".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[3].to_vec(), b"a3".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[0].to_vec(), b"a0-new".to_vec()), // overwrite
            put(META_NS, b"watermark".to_vec(), 2u64.to_be_bytes().to_vec()),  // overwrite
        ],
        vec![
            put(WALK_NS, height_key(3), b"h3".to_vec()),
            delete(SCATTERED_NS, SCATTERED_KEYS[1].to_vec()),
        ],
    ]
}

/// A commit sequence for the bulk properties. Append-only on the scattered
/// namespaces (each key once, no overwrite, no delete — a delete on a deferred
/// namespace is a contract violation), walk keys strictly ascending, meta freely
/// overwritten. Exactly two batches, so a restart property can split it cleanly.
pub(crate) fn bulk_batches() -> Vec<Vec<WriteOp>> {
    vec![
        vec![
            put(WALK_NS, height_key(0), b"h0".to_vec()),
            put(WALK_NS, height_key(1), b"h1".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[0].to_vec(), b"a0".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[1].to_vec(), b"a1".to_vec()),
            put(SCATTERED_NS_2, SCATTERED_KEYS[2].to_vec(), b"t-c".to_vec()),
            put(META_NS, b"watermark".to_vec(), 1u64.to_be_bytes().to_vec()),
        ],
        vec![
            put(WALK_NS, height_key(2), b"h2".to_vec()),
            put(WALK_NS, height_key(3), b"h3".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[2].to_vec(), b"a2".to_vec()),
            put(SCATTERED_NS, SCATTERED_KEYS[3].to_vec(), b"a3".to_vec()),
            put(SCATTERED_NS_2, SCATTERED_KEYS[0].to_vec(), b"t-a".to_vec()),
            put(META_NS, b"watermark".to_vec(), 2u64.to_be_bytes().to_vec()),
        ],
    ]
}

/// The final per-namespace contents of a direct build of `batches`: an
/// independent oracle. `Put` inserts or overwrites, `Delete` removes, exactly as
/// the backend must. The inner [`BTreeMap`] sorts keys bytewise, pinning `scan`
/// order; the outer [`HashMap`] only groups by namespace (order irrelevant).
pub(crate) type Oracle = HashMap<Namespace, BTreeMap<RawKey, RawValue>>;

/// Replay `batches` into an [`Oracle`].
pub(crate) fn oracle(batches: &[Vec<WriteOp>]) -> Oracle {
    let mut out: Oracle = HashMap::new();
    for batch in batches {
        for op in batch {
            match op {
                WriteOp::Put {
                    namespace,
                    key,
                    value,
                } => {
                    out.entry(*namespace)
                        .or_default()
                        .insert(key.clone(), value.clone());
                }
                WriteOp::Delete { namespace, key } => {
                    if let Some(map) = out.get_mut(namespace) {
                        map.remove(key);
                    }
                }
            }
        }
    }
    out
}

/// Assert every namespace in `specs` scans byte-for-byte equal to the oracle.
///
/// Because the oracle is a [`BTreeMap`] (sorted), this also pins `scan`'s
/// ascending-bytewise-order contract: an unsorted scan fails here.
pub(crate) fn assert_matches_oracle<B: Backend>(
    backend: &B,
    specs: &[NamespaceSpec],
    oracle: &Oracle,
) {
    let reader = backend.reader().expect("open reader");
    for spec in specs {
        let got = reader.scan(spec.namespace).expect("scan");
        let want: Vec<(RawKey, RawValue)> = oracle
            .get(&spec.namespace)
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        assert_eq!(
            got, want,
            "namespace {} differs from a direct build of the same commits",
            spec.namespace
        );
    }
}

/// Collect the keys `scan_range` visits over `[start, end_exclusive)`, in the
/// order visited.
pub(crate) fn range_keys<R: BackendReader>(
    reader: &R,
    namespace: Namespace,
    start: &[u8],
    end_exclusive: &[u8],
) -> Vec<RawKey> {
    let mut out = Vec::new();
    reader
        .scan_range(namespace, start, end_exclusive, &mut |key, _| {
            out.push(key.to_vec());
            ControlFlow::Continue(())
        })
        .expect("scan_range");
    out
}
