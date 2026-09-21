//! The ports, answered against a real database, checked against the reads they
//! replace.
//!
//! Every test here is differential: it asks the same question twice, once
//! through a `zaino-chain-store` port and once through the inherent read that
//! ChainIndex used before, and requires the two answers to agree. That shape is
//! deliberate. The port layer is ~1000 lines of conversion between what is on
//! disk and what a consumer sees, and a conversion has no self-evident correct
//! answer — but it does have a known-good one, because the inherent path has
//! been serving these answers for as long as the store has existed. Comparing
//! against it is the only check available that does not simply restate the
//! conversion in the assertion.
//!
//! The comparison stops being available when the inherent reads are deleted, by
//! which time these ports will have been the only path for a release. That is
//! the right moment to lose it, and not before.

use futures::StreamExt as _;
use zaino_chain_store::{
    ChainStoreReader, CompactBlockRead, PoolFilter, SpentOutputIndex, StoredBlockRead,
    TransactionIndex, TxOutSetIndex,
};
use zaino_primitives::types::{BlockTxPosition, Height as DomainHeight, TxIndex};
use zaino_proto::proto::utils::PoolTypeFilter;

use super::v1::load_vectors_v1db_and_reader;
use crate::adapter::{domain_outpoint, indexed_block_from_stored, stored_tx_out};
use crate::conversion::{compact_block_to_wire, pool_filter_from_wire};
use crate::tests::fixtures::indexed_block_chain;
use crate::types::{Height, Outpoint, TxLocation};

/// The domain height for a stored one, in a test that has already established
/// the height is real.
fn domain(height: Height) -> DomainHeight {
    DomainHeight::try_from(height.0).expect("vector heights are within the protocol maximum")
}

/// Blocks read through [`StoredBlockRead`] are the blocks the inherent read
/// returns.
///
/// The conversion runs twice here — out of the stored shape and back into it —
/// so the assertion covers both directions at once. Both matter: ChainIndex
/// reads through the first, and the freeze sink writes through the second, and
/// a field that survived one but not the other would make a block change shape
/// depending on which way it crossed the seam.
#[tokio::test(flavor = "multi_thread")]
async fn stored_blocks_match_the_inherent_read() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    for expected in indexed_block_chain(&data.blocks) {
        let height = expected.context.index.height;

        let mut chunk = StoredBlockRead::blocks_chunk(&reader, domain(height), domain(height))
            .await
            .expect("the store holds every vector height");
        assert_eq!(chunk.len(), 1, "a one-height chunk holds one block");

        let through_ports = indexed_block_from_stored(&chunk.pop().expect("checked above"))
            .expect("a block that came out of the store goes back into it");

        assert_eq!(
            through_ports, expected,
            "block at height {height} differs between the port and the oracle"
        );
    }
}

/// A multi-height chunk holds exactly the blocks in the range, ascending.
///
/// Separate from the single-block case because the chunked walk is a different
/// code path — one read transaction over a cursor rather than one per block —
/// and its ordering is a contract the port states.
#[tokio::test(flavor = "multi_thread")]
async fn a_block_chunk_covers_its_range_in_ascending_order() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;
    let expected: Vec<_> = indexed_block_chain(&data.blocks).collect();
    let last = expected.last().expect("the vectors hold blocks");

    let chunk = StoredBlockRead::blocks_chunk(&reader, domain(Height(0)), domain(last.height()))
        .await
        .expect("the whole vector range is below the watermark");

    assert_eq!(chunk.len(), expected.len());
    for (block, oracle) in chunk.iter().zip(&expected) {
        assert_eq!(
            u32::from(block.header.height),
            oracle.height().0,
            "chunk is not ascending, or skips a height"
        );
    }
}

/// The chunked stream yields every block in the range, once, ascending.
///
/// Chunk boundaries carry no meaning, so the assertion is over the flattened
/// sequence rather than over how it was divided.
#[tokio::test(flavor = "multi_thread")]
async fn the_block_stream_yields_the_whole_range_once() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;
    let expected: Vec<_> = indexed_block_chain(&data.blocks).collect();
    let last = expected.last().expect("the vectors hold blocks");

    // `pin!` rather than `Box::pin`: the port hands back an opaque, `!Unpin`
    // stream, and pinning it to the stack is what a consumer does instead of
    // paying for an allocation.
    let stream = StoredBlockRead::blocks_stream(&reader, domain(Height(0)), domain(last.height()))
        .await
        .expect("the whole vector range is below the watermark");
    let mut stream = std::pin::pin!(stream);

    let mut heights = Vec::new();
    while let Some(chunk) = stream.next().await {
        for block in chunk.expect("no chunk fails over the vector range") {
            heights.push(u32::from(block.header.height));
        }
    }

    let oracle: Vec<u32> = expected.iter().map(|block| block.height().0).collect();
    assert_eq!(heights, oracle);
}

/// Compact blocks read through the port, converted to the wire, are the wire
/// blocks the inherent read returns.
///
/// This is the byte-sensitive one. ChainIndex serves `GetBlockRange` from it,
/// so a field dropped or mis-routed here is a wallet that syncs wrong rather
/// than a test that fails — and the two pool filters are checked separately
/// because the filter is pushed into the read and selects which rows are
/// decoded at all.
#[tokio::test(flavor = "multi_thread")]
async fn compact_blocks_match_the_inherent_read() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    for filter in [PoolTypeFilter::default(), PoolTypeFilter::includes_all()] {
        let pools = pool_filter_from_wire(&filter);

        for block in indexed_block_chain(&data.blocks) {
            let height = block.context.index.height;

            let expected = reader
                .get_compact_block(height, filter.clone())
                .await
                .expect("the store holds every vector height");

            let mut chunk =
                CompactBlockRead::compact_chunk(&reader, domain(height), domain(height), pools)
                    .await
                    .expect("the store holds every vector height");
            assert_eq!(chunk.len(), 1);
            let through_ports = compact_block_to_wire(&chunk.pop().expect("checked above"));

            assert_eq!(
                through_ports, expected,
                "compact block at height {height} differs between the port and the inherent read"
            );
        }
    }
}

/// The compact stream yields the same blocks, in the same order, as the
/// inherent stream over the same range.
#[tokio::test(flavor = "multi_thread")]
async fn the_compact_stream_matches_the_inherent_stream() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;
    let last = indexed_block_chain(&data.blocks)
        .last()
        .expect("the vectors hold blocks")
        .height();

    let filter = PoolTypeFilter::includes_all();

    let mut inherent = reader
        .get_compact_block_stream(Height(0), last, filter.clone())
        .await
        .expect("the whole vector range is available");
    let mut expected = Vec::new();
    while let Some(block) = inherent.next().await {
        expected.push(block.expect("no block fails over the vector range"));
    }

    let chunks = CompactBlockRead::compact_stream(
        &reader,
        domain(Height(0)),
        domain(last),
        pool_filter_from_wire(&filter),
    )
    .await
    .expect("the whole vector range is below the watermark");
    let mut chunks = std::pin::pin!(chunks);

    let mut through_ports = Vec::new();
    while let Some(chunk) = chunks.next().await {
        for block in chunk.expect("no chunk fails over the vector range") {
            through_ports.push(compact_block_to_wire(&block));
        }
    }

    assert_eq!(through_ports, expected);
}

/// A transaction's position, and the txid at it, round-trip against the
/// inherent location reads.
#[tokio::test(flavor = "multi_thread")]
async fn transaction_positions_match_the_inherent_reads() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    for block in indexed_block_chain(&data.blocks) {
        for tx in block.transactions() {
            let txid = *tx.txid();

            let expected = reader
                .get_tx_location(&txid)
                .await
                .expect("the store indexes every vector transaction")
                .expect("every vector transaction is located");

            let position = TransactionIndex::tx_position(&reader, &txid.0.into())
                .await
                .expect("the store indexes every vector transaction")
                .expect("every vector transaction is located");

            assert_eq!(u32::from(position.height), expected.block_height());
            assert_eq!(
                position.tx_index,
                u32::from(expected.tx_index()),
                "position index disagrees with the stored location"
            );

            let at = TransactionIndex::txid_at(&reader, position)
                .await
                .expect("a position the store produced resolves")
                .expect("a position the store produced names a transaction");
            assert_eq!(<[u8; 32]>::from(at), txid.0);
        }
    }
}

/// A position past the end of a block names nothing, rather than failing.
///
/// The inherent read errors here and the port answers `None`; that difference
/// is the port's contract, so it is pinned rather than compared.
#[tokio::test(flavor = "multi_thread")]
async fn a_position_past_the_end_of_a_block_names_nothing() {
    let (_data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    let position = BlockTxPosition {
        height: domain(Height(1)),
        tx_index: TxIndex::from(u16::MAX),
    };
    assert_eq!(
        TransactionIndex::txid_at(&reader, position)
            .await
            .expect("asking about an absent position is not a failure"),
        None
    );
}

/// Spenders read through the port carry the same position *and* the txid the
/// inherent path needed a second read to get.
#[tokio::test(flavor = "multi_thread")]
async fn outpoint_spenders_match_the_inherent_reads() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    // Every outpoint the vector chain spends, plus every outpoint it creates —
    // so both the spent and the unspent answer are covered.
    let mut outpoints: Vec<Outpoint> = Vec::new();
    for block in indexed_block_chain(&data.blocks) {
        for tx in block.transactions() {
            outpoints.extend(tx.transparent().spent_outpoints());
            for index in 0..tx.transparent().outputs().len() {
                outpoints.push(Outpoint::new(tx.txid().0, index as u32));
            }
        }
    }
    assert!(
        !outpoints.is_empty(),
        "the vector chain moves transparent value"
    );

    let expected = reader
        .get_outpoint_spenders(outpoints.clone())
        .await
        .expect("the store carries the spent index");

    let domain_outpoints: Vec<_> = outpoints.iter().map(domain_outpoint).collect();
    let through_ports = SpentOutputIndex::outpoint_spenders(&reader, &domain_outpoints)
        .await
        .expect("the store carries the spent index");

    assert_eq!(through_ports.len(), expected.len());
    for ((spender, location), outpoint) in through_ports.iter().zip(&expected).zip(&outpoints) {
        match (spender, location) {
            (None, None) => {}
            (Some(spender), Some(location)) => {
                assert_eq!(
                    u32::from(spender.position.height),
                    location.block_height(),
                    "spender height disagrees for {outpoint:?}"
                );
                assert_eq!(spender.position.tx_index, u32::from(location.tx_index()));

                // The txid the port resolved must be the one the inherent path
                // would have looked up separately.
                let oracle = reader
                    .get_txid(*location)
                    .await
                    .expect("a location the store produced resolves");
                assert_eq!(<[u8; 32]>::from(spender.txid), oracle.0);
            }
            (port, inherent) => panic!(
                "spend status disagrees for {outpoint:?}: port {port:?}, inherent {inherent:?}"
            ),
        }
    }
}

/// Previous outputs, and the unspent view of them, agree with the inherent
/// read and with the spent index.
#[tokio::test(flavor = "multi_thread")]
async fn previous_and_unspent_outputs_match_the_inherent_reads() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    for block in indexed_block_chain(&data.blocks) {
        for tx in block.transactions() {
            for index in 0..tx.transparent().outputs().len() {
                let outpoint = Outpoint::new(tx.txid().0, index as u32);
                let domain = domain_outpoint(&outpoint);

                let expected = reader
                    .get_previous_output(outpoint)
                    .await
                    .expect("the store holds every vector output");
                let expected = stored_tx_out(&expected).expect("a stored output is expressible");

                let previous = SpentOutputIndex::previous_outputs(&reader, &[domain])
                    .await
                    .expect("the store holds every vector output");
                assert_eq!(previous, vec![Some(expected)]);

                // `unspent_output` is the composition the port adds: present,
                // and not spent. It must agree with asking those two separately.
                let spent = reader
                    .get_outpoint_spender(outpoint)
                    .await
                    .expect("the store carries the spent index")
                    .is_some();
                let unspent = SpentOutputIndex::unspent_output(&reader, domain)
                    .await
                    .expect("the store holds every vector output");
                assert_eq!(
                    unspent.is_none(),
                    spent,
                    "unspent view disagrees with the spent index for {outpoint:?}"
                );
            }
        }
    }
}

/// A transaction's transparent outputs, read by position, are the outputs the
/// inherent read returns.
#[tokio::test(flavor = "multi_thread")]
async fn transparent_outputs_match_the_inherent_read() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    for block in indexed_block_chain(&data.blocks) {
        for (index, tx) in block.transactions().iter().enumerate() {
            let location = TxLocation::new(
                block.height().0,
                u16::try_from(index).expect("vector blocks are small"),
            );
            // `None` where a transaction has no transparent row at all — a
            // shielded-only transaction has none — and that absence must agree
            // between the two paths as much as the contents do.
            let expected = reader
                .get_transparent(location)
                .await
                .expect("the store holds every vector transaction")
                .map(|transparent| {
                    transparent
                        .outputs()
                        .iter()
                        .map(|out| stored_tx_out(out).expect("a stored output is expressible"))
                        .collect::<Vec<_>>()
                });

            let position = BlockTxPosition {
                height: domain(block.height()),
                tx_index: TxIndex::from(u16::try_from(index).expect("vector blocks are small")),
            };
            let through_ports = SpentOutputIndex::transparent_outputs(&reader, position)
                .await
                .expect("the store holds every vector transaction");

            assert_eq!(
                through_ports,
                expected,
                "tx {} of block {} differs between the port and the inherent read",
                index,
                block.height()
            );
            let _ = tx;
        }
    }
}

/// The accumulator handed over by the port is the accumulator on disk.
///
/// Field-by-field rather than by equality, so a failure names which counter
/// moved. `hash_serialized` is the one that matters most: it is what
/// `gettxoutsetinfo` reports, and a consumer comparing two Zaino deployments
/// compares exactly these bytes.
#[tokio::test(flavor = "multi_thread")]
async fn the_txout_set_accumulator_matches_the_inherent_read() {
    let (_data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    let expected = reader
        .get_tx_out_set_info_accumulator()
        .await
        .expect("the store carries the txout-set index");
    let through_ports = TxOutSetIndex::txout_set(&reader)
        .await
        .expect("the store carries the txout-set index");

    assert_eq!(through_ports.hash_serialized, expected.hash_serialized);
    assert_eq!(through_ports.transactions, expected.transactions);
    assert_eq!(
        through_ports.transaction_outputs,
        expected.transaction_outputs
    );
    assert_eq!(through_ports.bytes_serialized, expected.bytes_serialized);
    assert_eq!(through_ports.total_zatoshis, expected.total_zatoshis);
}

/// Hashes and heights agree with the inherent reads in both directions.
#[tokio::test(flavor = "multi_thread")]
async fn hashes_and_heights_match_the_inherent_reads() {
    let (data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    for block in indexed_block_chain(&data.blocks) {
        let height = block.height();

        let expected = reader
            .get_block_hash(height)
            .await
            .expect("the store holds every vector height")
            .expect("every vector height has a hash");
        let through_ports = ChainStoreReader::block_hash(&reader, domain(height))
            .await
            .expect("the store holds every vector height")
            .expect("every vector height has a hash");
        assert_eq!(<[u8; 32]>::from(through_ports), expected.0);

        let back = ChainStoreReader::block_height(&reader, expected.0.into())
            .await
            .expect("the store holds every vector block")
            .expect("every vector block has a height");
        assert_eq!(u32::from(back), height.0);
    }
}

/// A read above the watermark is refused as above it, not answered as absent.
///
/// The distinction is the whole reason the error exists: a caller told "no such
/// block" would report absent data as absent chain, where "above the watermark"
/// tells it to ask the recent window instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_above_the_watermark_is_refused_rather_than_missed() {
    let (_data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    let watermark = ChainStoreReader::watermark(&reader)
        .tip
        .expect("a synced store has a tip");
    let above = DomainHeight::try_from(u32::from(watermark.height) + 1)
        .expect("one above the vector tip is a valid height");

    assert!(matches!(
        ChainStoreReader::block_hash(&reader, above).await,
        Err(zaino_chain_store::ChainStoreError::AboveWatermark { .. })
    ));
}

/// A range extending above the watermark is truncated, not refused.
///
/// The counterpart to the single read above: a caller merging with the recent
/// window asks both halves the same range and lets each answer what it holds,
/// so the store answering less than it was asked for is correct.
#[tokio::test(flavor = "multi_thread")]
async fn a_range_above_the_watermark_is_truncated() {
    let (_data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    let watermark = ChainStoreReader::watermark(&reader)
        .tip
        .expect("a synced store has a tip");
    let above = DomainHeight::try_from(u32::from(watermark.height) + 100)
        .expect("a hundred above the vector tip is a valid height");

    let chunk =
        CompactBlockRead::compact_chunk(&reader, watermark.height, above, PoolFilter::default())
            .await
            .expect("a range crossing the watermark is answered, not refused");

    assert_eq!(chunk.len(), 1, "only the height at the watermark is held");
}

/// A descending range is refused, because the port is ascending-only.
#[tokio::test(flavor = "multi_thread")]
async fn a_descending_range_is_refused() {
    let (_data, _dir, _db, reader) = load_vectors_v1db_and_reader().await;

    assert!(matches!(
        StoredBlockRead::blocks_chunk(&reader, domain(Height(2)), domain(Height(1))).await,
        Err(zaino_chain_store::ChainStoreError::InvalidRange { .. })
    ));
}

// ---------------------------------------------------------------------------
// The write path
// ---------------------------------------------------------------------------

/// A block read out of one store and frozen into another produces identical
/// rows.
///
/// The strongest check available on the port pair, because it closes the loop
/// the two halves form: [`StoredBlockRead`] must express everything the writer
/// needs, and [`ChainStoreFreezeSink`] must put all of it back. A field the
/// read drops is invisible in a read-only test — the value simply never
/// appears — and shows up only when something writes the result down, which is
/// exactly what a composer freezing beyond the reorg bound does.
///
/// It has already earned its place twice: it is what caught non-standard
/// address keys round-tripping to zeroes, and the per-pool value balances
/// round-tripping to `None`.
#[tokio::test(flavor = "multi_thread")]
async fn a_block_frozen_from_a_read_writes_identical_rows() {
    use zaino_chain_store::ChainStoreFreezeSink;

    let (data, _dir, source_db, reader) = load_vectors_v1db_and_reader().await;

    let last = indexed_block_chain(&data.blocks)
        .last()
        .expect("the vectors hold blocks")
        .height();
    let blocks = StoredBlockRead::blocks_chunk(&reader, domain(Height(0)), domain(last))
        .await
        .expect("the whole vector range is below the watermark");

    // A second, empty store, fed only through the freeze port.
    let (_target_dir, target) = super::v1::spawn_v1_zaino_db(
        crate::tests::fixtures::fake_validator_from_vectors(&data.blocks),
    )
    .await
    .expect("a second store spawns");
    let target = std::sync::Arc::new(target);
    target.wait_until_ready().await;

    ChainStoreFreezeSink::freeze(target.as_ref(), &blocks)
        .await
        .expect("every block frozen in order is accepted");

    let target_reader = target.to_reader();
    for expected in indexed_block_chain(&data.blocks) {
        let height = expected.context.index.height;
        let written = target_reader
            .get_chain_block_by_height(height)
            .await
            .expect("the frozen store holds every height");
        assert_eq!(
            written,
            Some(expected),
            "block at height {height} was not written back as it was read"
        );
    }

    let _ = source_db;
}

/// The watermark advances as the store builds itself.
///
/// Regression test. `build_to` is the only path the sync worker drives, and it
/// was the one path that never published a watermark — so a store that started
/// empty stayed at "no tip" no matter how many blocks it wrote, and every read
/// bounded by the watermark refused forever. Nothing noticed because nothing
/// read through those bounds yet.
///
/// Asserted through the port rather than through `db_height`, because the
/// watermark is what the ports answer against and `db_height` was right the
/// whole time.
#[tokio::test(flavor = "multi_thread")]
async fn building_the_store_advances_the_watermark() {
    use zaino_chain_store::ChainStoreIngest;

    let data = crate::tests::fixtures::load_test_vectors().expect("vectors load");
    let source = crate::tests::fixtures::fake_validator_from_vectors(&data.blocks);
    let (_dir, store) = super::v1::spawn_v1_zaino_db(source)
        .await
        .expect("a store spawns");
    let store = std::sync::Arc::new(store);
    store.wait_until_ready().await;

    let reader = store.to_reader();
    assert_eq!(
        ChainStoreReader::watermark(&reader).tip,
        None,
        "a store that has built nothing has no tip"
    );

    let target = DomainHeight::try_from(10).expect("10 is a valid height");
    ChainStoreIngest::build_to(store.as_ref(), target)
        .await
        .expect("the fake validator serves the vector chain");
    store.wait_until_synced().await;

    let tip = ChainStoreReader::watermark(&reader)
        .tip
        .expect("a store that has built blocks has a tip");
    assert_eq!(
        tip.height, target,
        "the watermark did not follow the build to its target"
    );

    // And the reads bounded by it now answer, which is the thing that was
    // actually broken.
    assert!(ChainStoreReader::block_hash(&reader, target)
        .await
        .expect("a height at the watermark is answerable")
        .is_some());
}

// ---------------------------------------------------------------------------
// Passthrough
// ---------------------------------------------------------------------------

/// A passthrough store answers above whatever it holds durably.
///
/// Regression test, and the invariant is easy to get backwards. The watermark
/// describes the *durable rows*, and a passthrough store has none — but it can
/// still answer, because the read goes to the validator. Bounding a passthrough
/// read by the watermark refuses questions the store can answer, and it does so
/// for the whole of a long initial sync, which is precisely when a node depends
/// on passthrough to stay useful.
///
/// It also pins the provenance, because the bound keys off it: a store whose
/// reads route to the ephemeral backend must report `Passthrough` even when its
/// primary is a persistent database, or the bound comes back.
#[tokio::test]
async fn a_passthrough_store_answers_above_its_durable_tip() {
    use zaino_chain_store::Provenance;

    let data = crate::tests::fixtures::load_test_vectors().expect("vectors load");
    let source = crate::tests::fixtures::fake_validator_from_vectors(&data.blocks);
    let (_dir, store) = super::ephemeral::spawn_ephemeral_finalised_state(source)
        .await
        .expect("an ephemeral store spawns");
    let store = std::sync::Arc::new(store);
    store.wait_until_ready().await;

    let reader = store.to_reader();
    let watermark = ChainStoreReader::watermark(&reader);
    assert_eq!(
        watermark.provenance,
        Provenance::Passthrough,
        "an ephemeral store serves passthrough reads"
    );

    // A height well above anything the store could hold durably. The validator
    // has it, so the store answers.
    let height = domain(Height(100));
    assert!(
        ChainStoreReader::block_hash(&reader, height)
            .await
            .expect("a passthrough read is not bounded by durable rows")
            .is_some(),
        "the passthrough store refused a height its validator holds"
    );

    // Ranges are not clamped either, for the same reason.
    let chunk = CompactBlockRead::compact_chunk(
        &reader,
        domain(Height(1)),
        domain(Height(20)),
        PoolFilter::default(),
    )
    .await
    .expect("a passthrough range is not bounded by durable rows");
    assert_eq!(chunk.len(), 20, "the passthrough range was clamped");
}
