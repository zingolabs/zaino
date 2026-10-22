//! `Backend::footprint` on a real filesystem (page cache = the kernel's, read by `mincore`)

#![cfg(target_os = "linux")]

use std::{
    fs,
    num::{NonZeroU32, NonZeroUsize},
};

use zaino_persistence::{
    sequence_key, Backend, BackendWriter, Namespace, NamespaceFootprint, NamespaceSpec, Width,
    WriteOp,
};
use zaino_persistence_lsm_tree::{LsmConfig, LsmTreeBackend, LsmTreeConfig, Tuning};
use zaino_primitives::types::{BlockHash, BlockRef, Height};

const FIXED: Namespace = Namespace::new("fixed");
const VARIABLE: Namespace = Namespace::new("variable");
const DROPPED: Namespace = Namespace::new("dropped");
const CACHED: Namespace = Namespace::new("cached");

/// - bytes = committed data (records + 8-byte ends, every listed segment), never the files'
///   preallocated tails
/// - cached right after the flush: each `cache_writes` namespace all of it; a default sequence
///   none (dropped once synced), a default map only what opening the view reads (summary +
///   filters)
#[test]
fn footprint_reports_each_namespaces_committed_bytes_and_their_page_cache_share() {
    let width = |n| NonZeroU32::new(n).expect("non-zero");
    let specs = [
        NamespaceSpec::sequence(FIXED, Width::Fixed(width(32))),
        NamespaceSpec::sequence(VARIABLE, Width::Variable),
        NamespaceSpec::map(DROPPED, width(36), width(8), false),
        NamespaceSpec::map(CACHED, width(36), width(8), false),
    ];
    let cached = Tuning {
        scope: 0,
        cache_writes: true,
    };
    let tuning = [(FIXED, cached), (CACHED, cached)];
    let dir = tempfile::tempdir().expect("tempdir");
    let config = LsmTreeConfig {
        name: "footprint",
        format: 1,
        namespaces: &specs,
        tuning: &tuning,
        lsm: LsmConfig::default(),
        write_buffer: NonZeroUsize::MAX,
    };
    let backend = LsmTreeBackend::open_real(dir.path(), &config).expect("open");
    let mut writer = backend.writer().expect("writer");
    for n in 0..20_000u64 {
        let mut key = vec![0u8; 36];
        key[..8].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
        let put = |namespace, key: Vec<u8>, value: Vec<u8>| WriteOp::Put {
            namespace,
            key,
            value,
        };
        let ops = vec![
            put(FIXED, sequence_key(n), vec![n as u8; 32]),
            put(
                VARIABLE,
                sequence_key(n),
                vec![n as u8; 100 + (n % 200) as usize],
            ),
            put(DROPPED, key.clone(), n.to_le_bytes().to_vec()),
            put(CACHED, key, n.to_le_bytes().to_vec()),
        ];
        let tip = BlockRef {
            height: Height::try_from(n as u32).expect("height"),
            hash: BlockHash::from([0; 32]),
        };
        writer.commit(tip, ops).expect("commit");
    }
    backend.flush().expect("flush");

    let segments = |map: &str| -> u64 {
        fs::read_dir(dir.path().join(map))
            .expect("map dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "seg"))
            .map(|entry| fs::metadata(entry.path()).expect("segment").len())
            .sum()
    };
    let (dropped, kept) = (segments("dropped"), segments("cached"));
    assert!(
        dropped > 0 && dropped == kept,
        "same rows, same segment bytes"
    );
    let variable: u64 = (0..20_000u64).map(|n| 100 + n % 200).sum::<u64>() + 20_000 * 8;
    let footprint = backend.footprint().expect("footprint");
    let bytes: Vec<(Namespace, u64)> = footprint
        .iter()
        .map(|namespace| (namespace.namespace, namespace.bytes))
        .collect();
    assert_eq!(
        bytes,
        [
            (FIXED, 20_000 * 32),
            (VARIABLE, variable),
            (DROPPED, dropped),
            (CACHED, kept)
        ],
        "committed bytes, never the preallocated tails"
    );

    // tmpfs: its page cache is its storage, every byte always resident
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    let filesystem = rustix::fs::statfs(dir.path()).expect("statfs");
    if filesystem.f_type as i64 == TMPFS_MAGIC {
        eprintln!("residency not checked: the temporary directory is on tmpfs");
        return;
    }
    let opened = footprint
        .iter()
        .find(|namespace| namespace.namespace == DROPPED)
        .and_then(|namespace| namespace.cached);
    assert!(
        opened.is_some_and(|bytes| bytes < dropped / 10),
        "dropped: {footprint:?}"
    );
    let expected: Vec<NamespaceFootprint> = [
        (FIXED, 20_000 * 32, None),
        (VARIABLE, variable, Some(0)),
        (DROPPED, dropped, opened),
        (CACHED, kept, None),
    ]
    .iter()
    .map(|&(namespace, bytes, cached)| NamespaceFootprint {
        namespace,
        bytes,
        cached: cached.or(Some(bytes)),
    })
    .collect();
    assert_eq!(footprint, expected);
}
