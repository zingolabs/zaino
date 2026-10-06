//! The non-bulk properties of the [`Backend`] contract: reads, writes,
//! ordering, isolation, and persistence across a reopen.

use super::support::{self, META_NS, SCATTERED_KEYS, SCATTERED_NS, SCATTERED_NS_2, WALK_NS};
use super::BackendFactory;
use crate::backend::{Backend, BackendReader, BackendWriter, RawKey, RawValue};
use crate::error::CommitError;

/// A committed put is readable; an overwrite replaces it; a delete removes it;
/// and a get of an absent key is `None`, not an error.
pub fn get_put_delete_round_trip<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    let key = SCATTERED_KEYS[0].to_vec();

    commit(
        &backend,
        vec![support::put(SCATTERED_NS, key.clone(), b"v1".to_vec())],
    );
    assert_eq!(
        get(&backend, SCATTERED_NS, &key),
        Some(b"v1".to_vec()),
        "put then get returns the value"
    );

    commit(
        &backend,
        vec![support::put(SCATTERED_NS, key.clone(), b"v2".to_vec())],
    );
    assert_eq!(
        get(&backend, SCATTERED_NS, &key),
        Some(b"v2".to_vec()),
        "a second put overwrites"
    );

    commit(&backend, vec![support::delete(SCATTERED_NS, key.clone())]);
    assert_eq!(
        get(&backend, SCATTERED_NS, &key),
        None,
        "delete removes the key"
    );

    assert_eq!(
        get(&backend, SCATTERED_NS, b"never-written"),
        None,
        "a get of an absent key is None, not an error"
    );
}

/// Every op of a single multi-namespace commit is visible after it returns —
/// the batch applies all-or-nothing, never partially.
pub fn commit_is_atomic_across_namespaces<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);

    commit(
        &backend,
        vec![
            support::put(WALK_NS, support::height_key(0), b"h0".to_vec()),
            support::put(SCATTERED_NS, SCATTERED_KEYS[1].to_vec(), b"a".to_vec()),
            support::put(SCATTERED_NS_2, SCATTERED_KEYS[2].to_vec(), b"t".to_vec()),
            support::put(META_NS, b"watermark".to_vec(), b"1".to_vec()),
        ],
    );

    assert_eq!(
        get(&backend, WALK_NS, &support::height_key(0)),
        Some(b"h0".to_vec())
    );
    assert_eq!(
        get(&backend, SCATTERED_NS, &SCATTERED_KEYS[1]),
        Some(b"a".to_vec())
    );
    assert_eq!(
        get(&backend, SCATTERED_NS_2, &SCATTERED_KEYS[2]),
        Some(b"t".to_vec())
    );
    assert_eq!(
        get(&backend, META_NS, b"watermark"),
        Some(b"1".to_vec()),
        "all four namespaces' ops are visible from the one commit"
    );
}

/// `scan` returns keys in ascending bytewise order — not insertion order, not
/// numeric order. The fixture keys are chosen so those three orders all differ.
pub fn scan_returns_bytewise_key_order<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    commit(&backend, SCATTERED_KEYS.iter().map(scattered_put).collect());

    let got: Vec<RawKey> = backend
        .reader()
        .expect("reader")
        .scan(SCATTERED_NS)
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();

    let bytewise = vec![
        vec![0x00, 0x02],
        vec![0x00, 0x10],
        vec![0x01, 0x01],
        vec![0x02, 0x00],
    ];
    assert_eq!(got, bytewise, "scan is ascending bytewise");

    // Guard that the fixture actually discriminates: bytewise order must match
    // neither insertion order nor little-endian numeric order, or the assertion
    // above would pass for the wrong reason.
    let insertion: Vec<RawKey> = SCATTERED_KEYS.iter().map(|k| k.to_vec()).collect();
    assert_ne!(
        bytewise, insertion,
        "fixture discriminates against insertion order"
    );
    let mut by_le = SCATTERED_KEYS;
    by_le.sort_by_key(|k| u16::from_le_bytes(*k));
    let little_endian: Vec<RawKey> = by_le.iter().map(|k| k.to_vec()).collect();
    assert_ne!(
        bytewise, little_endian,
        "fixture discriminates against little-endian numeric order"
    );
}

/// `scan_range` visits `[start, end_exclusive)` in ascending order: the low
/// bound is inclusive, the high bound exclusive, and empty or past-the-end
/// ranges visit nothing.
pub fn scan_range_is_ascending_and_half_open<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    commit(&backend, SCATTERED_KEYS.iter().map(scattered_put).collect());
    let reader = backend.reader().expect("reader");

    let (b, d, c, a) = (
        vec![0x00u8, 0x02],
        vec![0x00u8, 0x10],
        vec![0x01u8, 0x01],
        vec![0x02u8, 0x00],
    );

    assert_eq!(
        support::range_keys(&reader, SCATTERED_NS, &[0x00], &[0xff, 0xff]),
        vec![b.clone(), d.clone(), c.clone(), a.clone()],
        "a covering range returns every key in bytewise order"
    );
    assert_eq!(
        support::range_keys(&reader, SCATTERED_NS, &b, &a),
        vec![b.clone(), d.clone(), c.clone()],
        "low bound inclusive (b present), high bound exclusive (a dropped)"
    );
    assert_eq!(
        support::range_keys(&reader, SCATTERED_NS, &b, &c),
        vec![b.clone(), d.clone()],
        "the high bound is exclusive even when it names an existing key"
    );
    assert_eq!(
        support::range_keys(&reader, SCATTERED_NS, &d, &a),
        vec![d.clone(), c.clone()],
        "the low bound is inclusive at an interior key"
    );
    assert!(
        support::range_keys(&reader, SCATTERED_NS, &c, &c).is_empty(),
        "an empty range (start == end) visits nothing"
    );
    assert!(
        support::range_keys(&reader, SCATTERED_NS, &[0x03, 0x00], &[0xff, 0xff]).is_empty(),
        "a start past every key visits nothing"
    );
    assert!(
        support::range_keys(&reader, META_NS, &[0x00], &[0xff]).is_empty(),
        "a range over an empty namespace is not an error"
    );
}

/// `first_key` is the smallest bytewise key, or `None` when the namespace is
/// empty.
pub fn first_key_is_smallest_or_none<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    let reader = backend.reader().expect("reader");
    assert_eq!(
        reader.first_key(SCATTERED_NS).expect("first_key"),
        None,
        "an empty namespace has no first key"
    );

    commit(&backend, SCATTERED_KEYS.iter().map(scattered_put).collect());
    let reader = backend.reader().expect("reader");
    assert_eq!(
        reader.first_key(SCATTERED_NS).expect("first_key"),
        Some(vec![0x00, 0x02]),
        "first_key is the smallest bytewise key, not the first inserted"
    );
}

/// Namespaces are independent keyspaces: the same key carries different values
/// in different namespaces, a scan of one never returns another's entries, and a
/// delete in one leaves the others untouched.
pub fn namespaces_are_isolated<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    let key = SCATTERED_KEYS[0].to_vec();

    commit(
        &backend,
        vec![
            support::put(SCATTERED_NS, key.clone(), b"in-ns1".to_vec()),
            support::put(SCATTERED_NS_2, key.clone(), b"in-ns2".to_vec()),
        ],
    );

    assert_eq!(get(&backend, SCATTERED_NS, &key), Some(b"in-ns1".to_vec()));
    assert_eq!(
        get(&backend, SCATTERED_NS_2, &key),
        Some(b"in-ns2".to_vec())
    );

    let ns1_values: Vec<RawValue> = backend
        .reader()
        .expect("reader")
        .scan(SCATTERED_NS)
        .expect("scan")
        .into_iter()
        .map(|(_, value)| value)
        .collect();
    assert_eq!(
        ns1_values,
        vec![b"in-ns1".to_vec()],
        "a scan of one namespace never returns another's entries"
    );

    commit(&backend, vec![support::delete(SCATTERED_NS, key.clone())]);
    assert_eq!(get(&backend, SCATTERED_NS, &key), None);
    assert_eq!(
        get(&backend, SCATTERED_NS_2, &key),
        Some(b"in-ns2".to_vec()),
        "deleting in one namespace leaves the other"
    );
}

/// Committed, flushed data survives dropping the backend and reopening the same
/// storage. Skipped (a no-op) for a non-persistent backend, whose `reopen`
/// returns `None`.
pub fn reopen_persists_committed_data<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let batches = support::direct_batches();
    let expected = support::oracle(&batches);

    {
        let backend = factory.fresh(&specs);
        let mut writer = backend.writer().expect("writer");
        for batch in batches {
            writer.commit(batch).expect("commit");
        }
        backend.flush().expect("flush");
    }

    let Some(backend) = factory.reopen(&specs) else {
        return;
    };
    support::assert_matches_oracle(&backend, &specs, &expected);
}

/// A non-ascending put on a [`WalkOrdered`](crate::KeyOrder::WalkOrdered)
/// namespace is handled one of two contract-legal ways, and the suite accepts
/// either: a backend that append-orders walk-ordered writes MAY reject it with
/// [`CommitError::OutOfOrderAppend`] naming the namespace; a backend that does
/// not (it writes every namespace the same way) accepts it, and then the value
/// must be stored and readable like any other put.
///
/// This pins the [`KEY_ORDER`](crate::KeyOrder) contract from outside: the fact
/// enables append-ordering, it never forces it, so both the appending backend
/// (LMDB) and the order-agnostic one (in-memory) conform. A key that regresses
/// is the shape a mis-stated `WalkOrdered` codec would produce.
pub fn walk_ordered_rejects_or_stores_non_ascending_put<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    let mut writer = backend.writer().expect("writer");

    // Establish a high walk-ordered key, then commit a lower one on its own: a
    // key that regresses relative to what the namespace already holds.
    writer
        .commit(vec![support::put(
            WALK_NS,
            support::height_key(5),
            b"h5".to_vec(),
        )])
        .expect("an ascending walk-ordered put commits");
    let regressed = writer.commit(vec![support::put(
        WALK_NS,
        support::height_key(3),
        b"h3".to_vec(),
    )]);

    match regressed {
        Err(CommitError::OutOfOrderAppend { namespace, .. }) => assert_eq!(
            namespace,
            WALK_NS.to_string(),
            "the rejection names the walk-ordered namespace that regressed"
        ),
        Ok(()) => assert_eq!(
            get(&backend, WALK_NS, &support::height_key(3)),
            Some(b"h3".to_vec()),
            "a backend that accepts the non-ascending put must store its value"
        ),
        Err(other) => panic!("unexpected commit error for a non-ascending walk put: {other:?}"),
    }
}

/// Within a single commit, a [`WalkOrdered`](crate::KeyOrder::WalkOrdered)
/// namespace accepts its keys in any order — the engine extracts a batch's
/// blocks in parallel, so one namespace's puts arrive unsorted — as long as
/// every key is above the namespace's prior max. The commit succeeds, the scan
/// is ascending, and a key repeated within the commit holds the last value
/// written (plain-put last-write-wins).
///
/// This pins the contract an append-ordering backend must meet: it may not
/// require the caller to pre-sort a commit's ops, only that the keys advance
/// the namespace. An order-agnostic backend meets it trivially. Complements
/// [`walk_ordered_rejects_or_stores_non_ascending_put`], which covers a key at
/// or below the prior max *across* commits.
pub fn walk_ordered_accepts_shuffled_batch_with_last_write_wins<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    let mut writer = backend.writer().expect("writer");

    // Heights in neither ascending nor numeric order, all above the empty
    // namespace's (absent) prior max, with height 7 written twice — the later
    // value must win.
    writer
        .commit(vec![
            support::put(WALK_NS, support::height_key(5), b"h5".to_vec()),
            support::put(WALK_NS, support::height_key(9), b"h9".to_vec()),
            support::put(WALK_NS, support::height_key(7), b"h7-first".to_vec()),
            support::put(WALK_NS, support::height_key(2), b"h2".to_vec()),
            support::put(WALK_NS, support::height_key(7), b"h7-last".to_vec()),
        ])
        .expect("a shuffled walk-ordered batch, all keys above the prior max, commits");

    let got = backend
        .reader()
        .expect("reader")
        .scan(WALK_NS)
        .expect("scan");
    assert_eq!(
        got,
        vec![
            (support::height_key(2), b"h2".to_vec()),
            (support::height_key(5), b"h5".to_vec()),
            (support::height_key(7), b"h7-last".to_vec()),
            (support::height_key(9), b"h9".to_vec()),
        ],
        "scan is ascending and the key repeated in the commit holds the last value"
    );
}

/// Commit one batch, asserting nothing about return other than success.
fn commit<B: Backend>(backend: &B, ops: Vec<crate::backend::WriteOp>) {
    let mut writer = backend.writer().expect("writer");
    writer.commit(ops).expect("commit");
}

/// Read one key through a fresh reader.
fn get<B: Backend>(
    backend: &B,
    namespace: crate::backend::Namespace,
    key: &[u8],
) -> Option<RawValue> {
    backend
        .reader()
        .expect("reader")
        .get(namespace, key)
        .expect("get")
}

/// A put into [`SCATTERED_NS`] whose value echoes its key.
fn scattered_put(key: &[u8; 2]) -> crate::backend::WriteOp {
    support::put(SCATTERED_NS, key.to_vec(), key.to_vec())
}
