//! Where the per-block cost of serving a compact block goes.
//!
//! An ignored benchmark, run on demand over real blocks:
//!
//! ```text
//! ZAINO_BENCH_BLOCKS=blocks.hex ZAINO_BENCH_START=<first height> \
//!   cargo test --release -p zaino-lightserve bench_compact_block -- --ignored --nocapture
//! ```
//!
//! `blocks.hex` is one raw block per line (hex, as `getblock <h> 0` returns),
//! consecutive from the first height. The blocks are indexed through the real
//! sync engine into an LMDB store, then a block read is timed stage by stage,
//! mirroring the store's compose-on-read path: the seven index lookups, the
//! format-freshness check each lookup makes, the decodes, the whole store read
//! (whose remainder over those is assembly), the domain → wire conversion and
//! the protobuf encoding. Every stage runs over every block, warm, several
//! times; the report is nanoseconds per block.

use std::sync::Arc;
use std::time::Instant;

use prost::Message as _;
use tokio::sync::mpsc;
use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_indexer::{FetchConcurrency, FullBlocks, SourceProvisioner};
use zaino_indexes::index_set::IndexSet;
use zaino_indexes::indexes::chain_metadata::{self, ChainMetadataIndex};
use zaino_indexes::indexes::headers::{self, HeadersIndex};
use zaino_indexes::indexes::ironwood::{self, IronwoodIndex};
use zaino_indexes::indexes::orchard::{self, OrchardIndex};
use zaino_indexes::indexes::sapling::{self, SaplingIndex};
use zaino_indexes::indexes::transparent_data::{self, TransparentDataIndex};
use zaino_indexes::indexes::txids::{self, TxidsIndex};
use zaino_indexes::sets::current_zaino::context_from_block;
use zaino_indexes::sets::compact_blocks::CompactBlocks;
use zaino_persistence::{Backend, BackendReader, Namespace, NamespaceSpec};
use zaino_persistence_codec::{decode_value, encode_key, freshness, reserved_namespaces};
use zaino_primitives::types::{BlockSelector, ChainMetadata, CompactBlock, Height};
use zaino_service::{CompactBlockRead, TakeSnapshot};
use zaino_source::mock::MockChain;
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::StoreReader;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;

use crate::wire::ToWire;

/// How many times each stage walks every block.
const ROUNDS: u32 = 5;

/// Nanoseconds per block for `stage`, run `ROUNDS` times over `heights`.
fn per_block(heights: &[Height], mut stage: impl FnMut(Height)) -> f64 {
    let started = Instant::now();
    for _ in 0..ROUNDS {
        for &height in heights {
            stage(height);
        }
    }
    let blocks = f64::from(ROUNDS) * f64::from(u32::try_from(heights.len()).unwrap_or(u32::MAX));
    started.elapsed().as_secs_f64() * 1e9 / blocks
}

fn key(height: Height) -> Vec<u8> {
    encode_key::<HeadersIndex>(&BlockHeight::new(u64::from(height)))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: needs ZAINO_BENCH_BLOCKS, run with --release --ignored"]
async fn bench_compact_block() {
    let (Ok(path), Ok(start)) = (
        std::env::var("ZAINO_BENCH_BLOCKS"),
        std::env::var("ZAINO_BENCH_START"),
    ) else {
        eprintln!("set ZAINO_BENCH_BLOCKS and ZAINO_BENCH_START");
        return;
    };
    let start: u32 = start.trim().parse().expect("a start height");
    let raw = std::fs::read_to_string(path).expect("the blocks file");
    let blocks: Vec<_> = raw
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let bytes = hex::decode(line.trim()).expect("hex block");
            zaino_block_decode::decode_block(&bytes, ChainMetadata::ZERO).expect("a block")
        })
        .collect();
    let first = Height::try_from(start).expect("valid height");
    let last = Height::try_from(start + u32::try_from(blocks.len()).expect("few blocks") - 1)
        .expect("valid height");
    let raw_bytes: usize = raw.lines().map(|line| line.trim().len() / 2).sum();

    // Index the blocks through the real sync engine into LMDB.
    let dir = tempfile::tempdir().expect("a temp dir");
    let backend = LmdbBackend::open(LmdbConfig {
        path: dir.path().to_path_buf(),
        map_size_bytes: 8 << 30,
        namespaces: CompactBlocks::pipelines()
            .namespace_specs()
            .into_iter()
            .chain(reserved_namespaces().map(NamespaceSpec::meta))
            .collect(),
    })
    .expect("open lmdb");
    let mut chain = MockChain::new();
    for block in blocks {
        chain = chain.with_block(block);
    }
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));
    let mut engine = SyncEngine::from_pipelines(
        CompactBlocks::pipelines(),
        backend.clone(),
        EngineConfig {
            batch_size: 100,
            start_height: BlockHeight::new(u64::from(first)),
        },
    )
    .expect("valid index set");
    let provisioner = Arc::new(SourceProvisioner::<_, _, _, FullBlocks>::new(
        source,
        |block| context_from_block(&block),
        FetchConcurrency::SERIAL,
    ));
    let (tx, rx) = mpsc::channel(64);
    let feed = Arc::clone(&provisioner);
    let provision = tokio::spawn(async move { feed.provision(first, last, tx).await });
    engine.sync_channel(rx).await.expect("sync the blocks");
    provision.await.expect("joins").expect("provisions");

    let heights: Vec<Height> = (u32::from(first)..=u32::from(last))
        .map(|height| Height::try_from(height).expect("valid height"))
        .collect();
    let snapshot = StoreReader::<_, CompactBlocks>::new(Arc::new(backend.clone()))
        .snapshot()
        .await
        .expect("a snapshot");
    let reader = backend.reader().expect("a reader");
    let namespaces: [Namespace; 7] = [
        headers::ID.into(),
        chain_metadata::ID.into(),
        txids::ID.into(),
        transparent_data::ID.into(),
        sapling::ID.into(),
        orchard::ID.into(),
        ironwood::ID.into(),
    ];

    // Warm the map and collect the composed blocks for the wire stages.
    let mut composed: Vec<CompactBlock> = Vec::with_capacity(heights.len());
    for &height in &heights {
        composed.push(
            snapshot
                .compact_block(BlockSelector::Height(height))
                .await
                .expect("a read")
                .expect("an indexed block"),
        );
    }

    let lookups = per_block(&heights, |height| {
        let key = key(height);
        for namespace in namespaces {
            std::hint::black_box(reader.get(namespace, &key).expect("a get"));
        }
    });
    let freshness_checks = per_block(&heights, |_| {
        std::hint::black_box(freshness::<HeadersIndex>(&reader, namespaces[0]).expect("f"));
        std::hint::black_box(freshness::<ChainMetadataIndex>(&reader, namespaces[1]).expect("f"));
        std::hint::black_box(freshness::<TxidsIndex>(&reader, namespaces[2]).expect("f"));
        std::hint::black_box(freshness::<TransparentDataIndex>(&reader, namespaces[3]).expect("f"));
        std::hint::black_box(freshness::<SaplingIndex>(&reader, namespaces[4]).expect("f"));
        std::hint::black_box(freshness::<OrchardIndex>(&reader, namespaces[5]).expect("f"));
        std::hint::black_box(freshness::<IronwoodIndex>(&reader, namespaces[6]).expect("f"));
    });
    // Decode from bytes fetched up front, so this is decoding alone.
    let fetched: Vec<Vec<Vec<u8>>> = heights
        .iter()
        .map(|&height| {
            let key = key(height);
            namespaces
                .iter()
                .map(|&namespace| {
                    reader
                        .get(namespace, &key)
                        .expect("a get")
                        .expect("present")
                        .to_vec()
                })
                .collect()
        })
        .collect();
    let first_height = u32::from(first);
    let decodes = per_block(&heights, |height| {
        let at = usize::try_from(u32::from(height) - first_height).expect("an index");
        let bytes = &fetched[at];
        std::hint::black_box(decode_value::<HeadersIndex>(&bytes[0]).expect("d"));
        std::hint::black_box(decode_value::<ChainMetadataIndex>(&bytes[1]).expect("d"));
        std::hint::black_box(decode_value::<TxidsIndex>(&bytes[2]).expect("d"));
        std::hint::black_box(decode_value::<TransparentDataIndex>(&bytes[3]).expect("d"));
        std::hint::black_box(decode_value::<SaplingIndex>(&bytes[4]).expect("d"));
        std::hint::black_box(decode_value::<OrchardIndex>(&bytes[5]).expect("d"));
        std::hint::black_box(decode_value::<IronwoodIndex>(&bytes[6]).expect("d"));
    });
    let runtime = tokio::runtime::Handle::current();
    let whole_read = tokio::task::block_in_place(|| {
        per_block(&heights, |height| {
            std::hint::black_box(
                runtime
                    .block_on(snapshot.compact_block(BlockSelector::Height(height)))
                    .expect("a read"),
            );
        })
    });
    let clones = per_block(&heights, |height| {
        let at = usize::try_from(u32::from(height) - first_height).expect("an index");
        std::hint::black_box(composed[at].clone());
    });
    let to_wire = per_block(&heights, |height| {
        let at = usize::try_from(u32::from(height) - first_height).expect("an index");
        std::hint::black_box(composed[at].clone().to_wire());
    }) - clones;
    let wired: Vec<_> = composed.iter().cloned().map(ToWire::to_wire).collect();
    let encode = per_block(&heights, |height| {
        let at = usize::try_from(u32::from(height) - first_height).expect("an index");
        std::hint::black_box(wired[at].encode_to_vec());
    });
    let wire_bytes: usize = wired.iter().map(|block| block.encoded_len()).sum();

    let blocks = heights.len();
    println!(
        "blocks {blocks} ({first}..={last}), raw {} B/block, compact {} B/block",
        raw_bytes / blocks,
        wire_bytes / blocks
    );
    println!("ns/block  lookups (7 gets)            {lookups:>10.0}");
    println!("ns/block  freshness checks (7)        {freshness_checks:>10.0}");
    println!("ns/block  decodes (7)                 {decodes:>10.0}");
    println!("ns/block  whole store read            {whole_read:>10.0}");
    println!(
        "ns/block    of which assembly + rest    {:>10.0}",
        whole_read - lookups - freshness_checks - decodes
    );
    println!("ns/block  domain -> wire              {to_wire:>10.0}");
    println!("ns/block  protobuf encode             {encode:>10.0}");
    println!(
        "ns/block  TOTAL (read + wire + encode) {:>10.0}",
        whole_read + to_wire + encode
    );
}
