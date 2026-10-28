use std::num::{NonZeroU32, NonZeroUsize};
use std::ops::Bound::{self, Excluded, Included, Unbounded};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use zaino_primitives::types::{BlockHash, BlockRef, Height};

use super::InMemoryBackend;
use crate::backend::{
    raw_entry, sequence_key, Backend, BackendReader, BackendWriter, MaintenanceReader, Namespace,
    NamespaceSpec, RawKey, RawValue,
    ScanDirection::{self, Forward, Reverse},
    ScanRange, Width, WriteOp,
};
use crate::error::{CommitError, OpenError, ReadError};

/// Variable-width sequence: one record per position, any length.
const SEQ: Namespace = Namespace::new("seq");
/// One-byte keys to one-byte values, removable.
const MAP: Namespace = Namespace::new("map");
const OTHER: Namespace = Namespace::new("other");

const ONE: NonZeroU32 = NonZeroU32::MIN;

fn specs() -> [NamespaceSpec; 2] {
    [
        NamespaceSpec::sequence(SEQ, Width::Variable),
        NamespaceSpec::map(MAP, ONE, ONE, true),
    ]
}

fn tip(height: u32) -> BlockRef {
    BlockRef {
        hash: BlockHash::from([0; 32]),
        height: Height::try_from(height).expect("test heights are small"),
    }
}

/// Record at `position` of `value_len` bytes.
fn append(position: u8, value_len: usize) -> WriteOp {
    WriteOp::Put {
        namespace: SEQ,
        key: sequence_key(u64::from(position)),
        value: vec![position; value_len],
    }
}

fn insert(key: u8) -> WriteOp {
    WriteOp::Put {
        namespace: MAP,
        key: vec![key],
        value: vec![key],
    }
}

fn empty() -> InMemoryBackend {
    InMemoryBackend::new(&specs()).expect("specs are distinct")
}

fn commit(backend: &InMemoryBackend, height: u32, ops: Vec<WriteOp>) {
    backend
        .writer()
        .expect("in-memory writer is infallible")
        .commit(tip(height), ops)
        .expect("ops obey the rules");
}

/// `SEQ` holding positions `0..lens.len()`, record `i` of `lens[i]` bytes.
fn sequence_with(lens: &[usize]) -> InMemoryBackend {
    let backend = empty();
    let ops = lens
        .iter()
        .enumerate()
        .map(|(at, &len)| append(at as u8, len))
        .collect();
    commit(&backend, 0, ops);
    backend
}

fn range(
    lower: Bound<u8>,
    upper: Bound<u8>,
    direction: ScanDirection,
    chunk_bytes: usize,
) -> ScanRange {
    let key = |position: u8| sequence_key(u64::from(position));
    ScanRange {
        lower: lower.map(key),
        upper: upper.map(key),
        direction,
        chunk_bytes: NonZeroUsize::new(chunk_bytes).expect("test budgets are non-zero"),
    }
}

fn raw_chunks(
    backend: &InMemoryBackend,
    namespace: Namespace,
    range: ScanRange,
) -> Vec<Vec<(RawKey, RawValue)>> {
    backend
        .reader()
        .expect("in-memory reader is infallible")
        .scan_range(namespace, range, raw_entry)
        .collect::<Result<_, _>>()
        .expect("in-memory scan is infallible")
}

/// Last key byte: the position (positions < 256) or the map key.
fn chunk_keys(chunks: &[Vec<(RawKey, RawValue)>]) -> Vec<Vec<u8>> {
    chunks
        .iter()
        .map(|chunk| {
            chunk
                .iter()
                .filter_map(|(k, _)| k.last().copied())
                .collect()
        })
        .collect()
}

fn keys(chunks: &[Vec<(RawKey, RawValue)>]) -> Vec<u8> {
    chunk_keys(chunks).concat()
}

#[test]
fn visits_keys_in_direction_order() {
    let backend = empty();
    commit(&backend, 0, [3, 1, 4, 0, 2].map(insert).into());
    let all = |dir| {
        keys(&raw_chunks(
            &backend,
            MAP,
            ScanRange {
                lower: Unbounded,
                upper: Unbounded,
                direction: dir,
                chunk_bytes: NonZeroUsize::new(1024).expect("non-zero"),
            },
        ))
    };
    assert_eq!(all(Forward), [0, 1, 2, 3, 4]);
    assert_eq!(all(Reverse), [4, 3, 2, 1, 0]);
}

#[test]
fn bounds_select_keys() {
    let backend = sequence_with(&[1; 6]);
    let cases: [(Bound<u8>, Bound<u8>, &[u8]); 5] = [
        (Included(1), Included(4), &[1, 2, 3, 4]),
        (Excluded(1), Excluded(4), &[2, 3]),
        (Unbounded, Excluded(2), &[0, 1]),
        (Included(4), Unbounded, &[4, 5]),
        (Excluded(2), Included(3), &[3]),
    ];
    for (lower, upper, expected) in cases {
        let forward = keys(&raw_chunks(
            &backend,
            SEQ,
            range(lower, upper, Forward, 1024),
        ));
        let mut reverse = keys(&raw_chunks(
            &backend,
            SEQ,
            range(lower, upper, Reverse, 1024),
        ));
        reverse.reverse();
        assert_eq!(forward, expected, "forward {lower:?}..{upper:?}");
        assert_eq!(reverse, expected, "reverse {lower:?}..{upper:?}");
    }
}

#[test]
fn chunks_stay_within_byte_budget() {
    // Every entry is 10 raw bytes (8-byte key), so a 25-byte budget fits two.
    let backend = sequence_with(&[2; 10]);
    let chunks = raw_chunks(&backend, SEQ, range(Unbounded, Unbounded, Forward, 25));
    assert_eq!(
        chunk_keys(&chunks),
        [vec![0, 1], vec![2, 3], vec![4, 5], vec![6, 7], vec![8, 9]]
    );
}

#[test]
fn oversized_entry_is_returned_alone() {
    let backend = sequence_with(&[1, 100, 1]);
    for (dir, expected) in [
        (Forward, [vec![0], vec![1], vec![2]]),
        (Reverse, [vec![2], vec![1], vec![0]]),
    ] {
        let chunks = raw_chunks(&backend, SEQ, range(Unbounded, Unbounded, dir, 10));
        assert_eq!(chunk_keys(&chunks), expected, "{dir:?}");
    }
}

#[test]
fn joined_chunks_equal_full_scan() {
    let lens: Vec<usize> = (0..50).map(|k| k % 7 + 1).collect();
    let backend = sequence_with(&lens);
    let full: Vec<(RawKey, RawValue)> = lens
        .iter()
        .enumerate()
        .map(|(at, &len)| (sequence_key(at as u64), vec![at as u8; len]))
        .collect();
    let mut full_reverse = full.clone();
    full_reverse.reverse();
    for budget in 1..=60 {
        for (dir, expected) in [(Forward, &full), (Reverse, &full_reverse)] {
            let chunks = raw_chunks(&backend, SEQ, range(Unbounded, Unbounded, dir, budget));
            assert!(
                chunks.iter().all(|c| !c.is_empty()),
                "empty chunk at {budget} {dir:?}"
            );
            assert_eq!(&chunks.concat(), expected, "budget {budget} {dir:?}");
        }
    }
}

#[test]
fn filter_maps_and_skips_entries() {
    // Every entry is 11 raw bytes and the budget counts every visited one: two per chunk.
    let backend = sequence_with(&[3; 10]);
    let scan = |keep: fn(u8) -> bool| -> Vec<Vec<(u8, usize)>> {
        backend
            .reader()
            .expect("in-memory reader is infallible")
            .scan_range(
                SEQ,
                range(Unbounded, Unbounded, Forward, 22),
                move |key: &[u8], value: &[u8]| {
                    let position = key.last().copied()?;
                    keep(position).then_some((position, value.len()))
                },
            )
            .collect::<Result<_, _>>()
            .expect("in-memory scan is infallible")
    };
    assert_eq!(
        scan(|position| position.is_multiple_of(2)),
        [
            vec![(0, 3)],
            vec![(2, 3)],
            vec![(4, 3)],
            vec![(6, 3)],
            vec![(8, 3)]
        ]
    );
    assert_eq!(
        scan(|_| false),
        vec![Vec::new(); 5],
        "a filter keeping nothing still visits two entries per chunk"
    );
}

#[test]
fn filter_runs_once_per_entry() {
    let backend = sequence_with(&[1; 20]);
    for dir in [Forward, Reverse] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let kept: usize = backend
            .reader()
            .expect("in-memory reader is infallible")
            .scan_range(
                SEQ,
                range(Unbounded, Unbounded, dir, 18),
                move |key: &[u8], _: &[u8]| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    key.last()
                        .is_some_and(|p| p.is_multiple_of(3))
                        .then_some(())
                },
            )
            .map(|chunk| chunk.expect("in-memory scan is infallible").len())
            .sum();
        assert_eq!(kept, 7, "{dir:?}");
        assert_eq!(calls.load(Ordering::Relaxed), 20, "{dir:?}");
    }
}

#[test]
fn empty_and_inverted_ranges_yield_nothing() {
    let backend = sequence_with(&[1; 5]);
    let cases = [
        (Included(3), Included(2)),
        (Excluded(2), Excluded(2)),
        (Excluded(2), Included(2)),
        (Included(2), Excluded(2)),
        (Included(10), Included(20)),
    ];
    for (lower, upper) in cases {
        for dir in [Forward, Reverse] {
            let chunks = raw_chunks(&backend, SEQ, range(lower, upper, dir, 1024));
            assert!(chunks.is_empty(), "{lower:?}..{upper:?} {dir:?}");
        }
    }
}

#[test]
fn undeclared_namespace_is_an_error_for_every_read() {
    let reader = empty().reader().expect("in-memory reader is infallible");
    let not_found = |result: Result<_, ReadError>| matches!(result, Err(ReadError::NamespaceNotFound(name)) if name == OTHER.as_str());
    assert!(not_found(reader.get(OTHER, &[0]).map(drop)));
    assert!(not_found(reader.scan(OTHER).map(drop)));
    let mut chunks =
        reader.scan_range(OTHER, range(Unbounded, Unbounded, Forward, 1024), raw_entry);
    assert!(not_found(
        chunks.next().expect("error is yielded").map(drop)
    ));
    assert!(chunks.next().is_none(), "finished after the error");
    let empty_range = range(Included(1), Excluded(1), Forward, 1024);
    let mut chunks = reader.scan_range(OTHER, empty_range, raw_entry);
    assert!(
        not_found(
            chunks
                .next()
                .expect("error even for an empty range")
                .map(drop)
        ),
        "an undeclared namespace is an error whatever the range"
    );
}

#[test]
fn declared_empty_namespace_reads_as_empty() -> Result<(), ReadError> {
    let reader = empty().reader().expect("in-memory reader is infallible");
    assert_eq!(reader.tip()?, None);
    assert_eq!(reader.get(SEQ, &sequence_key(0))?, None);
    assert!(reader.scan(SEQ)?.is_empty());
    let mut chunks = reader.scan_range(SEQ, range(Unbounded, Unbounded, Forward, 1024), raw_entry);
    assert!(chunks.next().is_none());
    Ok(())
}

#[test]
fn a_namespace_declared_twice_is_refused() {
    let twice = [
        NamespaceSpec::sequence(SEQ, Width::Variable),
        NamespaceSpec::map(SEQ, ONE, ONE, false),
    ];
    let refused = InMemoryBackend::new(&twice).map(drop);
    assert!(
        matches!(&refused, Err(OpenError::DuplicateNamespace(name)) if name == SEQ.as_str()),
        "{refused:?}"
    );
}

#[test]
fn commit_naming_undeclared_namespace_applies_nothing() {
    let backend = empty();
    let mut writer = backend.writer().expect("in-memory writer is infallible");
    for undeclared in [
        WriteOp::Put {
            namespace: OTHER,
            key: vec![1],
            value: vec![1],
        },
        WriteOp::Delete {
            namespace: OTHER,
            key: vec![1],
        },
    ] {
        let result = writer.commit(tip(0), vec![append(0, 1), undeclared]);
        assert!(
            matches!(&result, Err(CommitError::NamespaceNotFound(name)) if name == OTHER.as_str()),
            "{result:?}"
        );
        let reader = backend.reader().expect("in-memory reader is infallible");
        assert!(
            reader.scan(SEQ).expect("declared").is_empty(),
            "rejected batch applied nothing"
        );
    }
}

#[test]
fn a_reader_and_its_chunks_keep_their_snapshot() -> Result<(), ReadError> {
    let backend = empty();
    commit(&backend, 0, [0, 2, 4, 6].map(insert).into());
    let reader = backend.reader().expect("in-memory reader is infallible");
    let all = ScanRange {
        lower: Unbounded,
        upper: Unbounded,
        direction: Forward,
        chunk_bytes: NonZeroUsize::new(2).expect("non-zero"),
    };
    let mut chunks = reader.scan_range(MAP, all.clone(), raw_entry);
    let first = chunks
        .next()
        .expect("range is non-empty")
        .expect("in-memory scan is infallible");
    assert_eq!(keys(&[first]), [0]);

    commit(
        &backend,
        1,
        vec![
            insert(1),
            insert(5),
            WriteOp::Delete {
                namespace: MAP,
                key: vec![4],
            },
        ],
    );

    let rest: Vec<_> = chunks.collect::<Result<_, _>>()?;
    assert_eq!(keys(&rest), [2, 4, 6], "chunks: the snapshot");
    assert_eq!(reader.tip()?, Some(tip(0)), "reader: the snapshot's tip");
    assert_eq!(
        reader.get(MAP, &[4])?,
        Some(vec![4]),
        "reader: the snapshot's rows"
    );
    assert_eq!(reader.get(MAP, &[1])?, None, "reader: later commits unseen");

    let fresh = backend.reader().expect("in-memory reader is infallible");
    assert_eq!(fresh.tip()?, Some(tip(1)), "new reader: the latest tip");
    let latest: Vec<_> = fresh
        .scan_range(MAP, all, raw_entry)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        keys(&latest),
        [0, 1, 2, 5, 6],
        "new reader: the latest rows"
    );
    Ok(())
}
