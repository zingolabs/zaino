//! The buffer: records and entries per namespace, the cancel rule, snapshots unaffected by later
//! commits

use std::num::NonZeroU32;

use zaino_persistence::{sequence_key, Namespace, Width};
use zaino_primitives::types::{BlockHash, Height};

use super::*;

const SEQ: Namespace = Namespace::new("seq");
const MAP: Namespace = Namespace::new("map");

fn width(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("non-zero")
}

fn specs() -> [NamespaceSpec; 2] {
    [
        NamespaceSpec::sequence(SEQ, Width::Variable),
        NamespaceSpec::map(MAP, width(2), width(1), true),
    ]
}

fn tip(n: u32) -> BlockRef {
    BlockRef {
        height: Height::try_from(n).expect("height"),
        hash: BlockHash::from([0; 32]),
    }
}

fn put(namespace: Namespace, key: &[u8], value: &[u8]) -> WriteOp {
    WriteOp::Put {
        namespace,
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn delete(key: &[u8]) -> WriteOp {
    WriteOp::Delete {
        namespace: MAP,
        key: key.to_vec(),
    }
}

/// - records in order, entries in key order
/// - a buffered insert then removal = cancelled: its value written, its tombstone listed for a
///   newer segment; a removal of a durable key = tombstone row
/// - an earlier snapshot never sees a later commit
#[test]
fn commits_buffer_in_order_cancel_buffered_pairs_and_leave_snapshots_alone() {
    let specs = specs();
    let empty = Buffer::empty(&specs);
    let first = empty
        .with(
            &specs,
            tip(1),
            &[
                put(SEQ, &sequence_key(0), b"zero"),
                put(MAP, b"bb", b"2"),
                put(MAP, b"aa", b"1"),
            ],
        )
        .expect("first");
    let second = first
        .with(
            &specs,
            tip(2),
            &[
                put(SEQ, &sequence_key(1), b""),
                delete(b"aa"),
                delete(b"cc"),
            ],
        )
        .expect("second");

    let records: Vec<&[u8]> = second
        .records(0)
        .expect("a sequence")
        .iter()
        .map(Bytes::as_ref)
        .collect();
    assert_eq!(records, [&b"zero"[..], b""]);
    let (rows, cancelled) = second.map_rows(1);
    assert_eq!(
        rows,
        [
            (&b"aa"[..], Some(&b"1"[..])),
            (&b"bb"[..], Some(&b"2"[..])),
            (&b"cc"[..], None)
        ],
        "aa's value kept, cc a tombstone, key order"
    );
    assert_eq!(cancelled, [&b"aa"[..]], "aa's tombstone: a newer segment");
    let entries = second.entries(1).expect("a map");
    assert_eq!(
        entries.get(&b"aa"[..]),
        Some(&Entry::Cancelled(Bytes::from_static(b"1")))
    );
    assert_eq!(entries.get(&b"cc"[..]), Some(&Entry::Removed));
    assert_eq!(second.tip(), Some(tip(2)));
    assert!(second.heap() > first.heap() && first.heap() > 0);
    assert_eq!(second.item_bytes(), 4 + 2 + 1 + 2 + 1 + 2 + 2);

    assert_eq!(
        first.records(0).expect("a sequence").len(),
        1,
        "first unchanged"
    );
    assert_eq!(
        first.map_rows(1),
        (
            vec![(&b"aa"[..], Some(&b"1"[..])), (&b"bb"[..], Some(&b"2"[..]))],
            Vec::new()
        )
    );
    assert_eq!((empty.tip(), empty.heap()), (None, 0));
    assert_eq!(empty.records(1), None, "a map is not a sequence");
    assert_eq!(empty.entries(0), None, "a sequence is not a map");
}

/// Breaches the rules already refuse = `Internal`, nothing changed
#[test]
fn ops_the_rules_refuse_are_internal_errors() {
    let specs = specs();
    let held = Buffer::empty(&specs)
        .with(&specs, tip(1), &[put(MAP, b"aa", b"1")])
        .expect("held");
    let removed = held
        .with(&specs, tip(2), &[delete(b"aa")])
        .expect("removed");
    let cases = [
        (&held, put(Namespace::new("other"), b"aa", b"1")),
        (
            &held,
            WriteOp::Delete {
                namespace: SEQ,
                key: sequence_key(0),
            },
        ),
        (&removed, delete(b"aa")),
    ];
    for (buffer, op) in cases {
        let refused = buffer.with(&specs, tip(3), std::slice::from_ref(&op));
        assert!(
            matches!(refused, Err(StoreError::Internal(_))),
            "{op:?}: {refused:?}"
        );
    }
}
