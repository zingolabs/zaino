//! The real sync engine builds the UTXO-set summary cross index, and each
//! block's stored delta equals what an independent UTXO-set model says the block
//! did.
//!
//! The model keeps the actual set in a map and counts from it, so it shares
//! none of the index's logic: no spend ordering, no sibling reads. The chain
//! covers the cases that logic exists for — an output outside the set created
//! and later spent, an output created and spent in one block, a transaction
//! emptied by spends in two different batches, and one emptied by spends at two
//! heights of the same batch (where the dependency overlay already shows the
//! later spend to the earlier block's extraction).

use std::collections::HashMap;

use zaino_indexes::indexes::transparent_data::{TransparentDataIndex, TransparentTxCompact};
use zaino_indexes::indexes::transparent_spends::TransparentSpendsIndex;
use zaino_indexes::indexes::txid_location::TxidLocationIndex;
use zaino_indexes::indexes::txout_set::{
    canonical_entry, TxOutSetDelta, TxOutSetIndex, DOMAIN_TAG, ENTRY_LEN,
};
use zaino_indexes::sets::current_zaino::CurrentZainoContext;
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, BackendReader, Namespace};
use zaino_persistence_codec::decode_value;
use zaino_primitives::types::{
    BlockHash, CompactDifficulty, OutputIndex, Script, TransactionId, Zatoshis,
};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::index_pipelines::IndexPipelines;
use zaino_sync::primitives::BlockHeight;

fn txid(id: u8) -> TransactionId {
    TransactionId::from([id; 32])
}

fn p2pkh(seed: u8) -> Script {
    let mut bytes = vec![0x76, 0xa9, 0x14];
    bytes.extend([seed; 20]);
    bytes.extend([0x88, 0xac]);
    Script::new(bytes)
}

fn p2sh(seed: u8) -> Script {
    let mut bytes = vec![0xa9, 0x14];
    bytes.extend([seed; 20]);
    bytes.push(0x87);
    Script::new(bytes)
}

/// An `OP_RETURN` script: not P2PKH or P2SH, so outside the set.
fn op_return() -> Script {
    Script::new(vec![0x6a, 0x02, 0xab, 0xcd])
}

fn zat(amount: u64) -> Zatoshis {
    Zatoshis::new(amount).expect("valid amount")
}

/// One test transaction: its id, the outpoints it spends, the outputs it makes.
struct Tx {
    id: u8,
    inputs: Vec<(u8, OutputIndex)>,
    outputs: Vec<(u64, Script)>,
}

/// The coinbase's null outpoint.
const COINBASE: (u8, OutputIndex) = (0, OutputIndex::MAX);

fn block(height: u64, txs: &[Tx]) -> CurrentZainoContext {
    let block_height = BlockHeight::new(height);
    let mut spends = Vec::new();
    let mut txid_locations = Vec::new();
    let mut transparent_txs = Vec::new();
    for (position, tx) in txs.iter().enumerate() {
        let position = u32::try_from(position).expect("test block is small");
        txid_locations.push((txid(tx.id), block_height, position));
        for &(prev, index) in &tx.inputs {
            if (prev, index) != COINBASE {
                spends.push((txid(prev), index, txid(tx.id)));
            }
        }
        transparent_txs.push(TransparentTxCompact {
            inputs: tx
                .inputs
                .iter()
                .map(|&(prev, index)| (txid(prev), index))
                .collect(),
            outputs: tx
                .outputs
                .iter()
                .map(|(value, script)| (zat(*value), script.clone()))
                .collect(),
        });
    }
    CurrentZainoContext {
        height: block_height,
        hash: BlockHash::from([u8::try_from(height).expect("test height < 256") + 1; 32]),
        prev_hash: BlockHash::ZERO,
        time: 0,
        bits: CompactDifficulty::try_from_bits(0x1d00_ffff).expect("valid nBits"),
        txids: txs.iter().map(|tx| txid(tx.id)).collect(),
        spends,
        txid_locations,
        transparent_txs,
        sapling_txs: Vec::new(),
        orchard_txs: Vec::new(),
        ironwood_txs: Vec::new(),
    }
}

/// The commitment digest of one set member, computed here from the public
/// contract rather than through the index.
fn digest(id: u8, index: OutputIndex, value: u64, script: &Script) -> [u8; 32] {
    let bytes = script.as_bytes();
    let (hash, tag) = if bytes.first() == Some(&0x76) {
        (&bytes[3..23], 0x00)
    } else {
        (&bytes[2..22], 0x01)
    };
    let entry = canonical_entry(
        txid(id),
        index,
        zat(value),
        hash.try_into().expect("20-byte hash"),
        tag,
    );
    let hashed = blake2b_simd::Params::new()
        .hash_length(32)
        .to_state()
        .update(DOMAIN_TAG)
        .update(&entry)
        .finalize();
    hashed.as_bytes().try_into().expect("32-byte digest")
}

fn in_set(script: &Script) -> bool {
    matches!(script.as_bytes().first(), Some(0x76 | 0xa9))
}

/// The independent model: the actual UTXO set, and each block's delta read off
/// how it changed.
fn model(chain: &[Vec<Tx>]) -> Vec<TxOutSetDelta> {
    // (txid, index) -> (value, script), set members only.
    let mut set: HashMap<(u8, OutputIndex), (u64, Script)> = HashMap::new();
    let holders = |set: &HashMap<(u8, OutputIndex), (u64, Script)>| {
        let mut ids: Vec<u8> = set.keys().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        u64::try_from(ids.len()).expect("small")
    };

    let mut deltas = Vec::new();
    for txs in chain {
        let mut delta = TxOutSetDelta::EMPTY;
        let xor = |delta: &mut TxOutSetDelta, d: [u8; 32]| {
            for (byte, other) in delta.digest.iter_mut().zip(d) {
                *byte ^= other;
            }
        };
        for tx in txs {
            let before = holders(&set);
            let mut created = false;
            for (index, (value, script)) in tx.outputs.iter().enumerate() {
                let index = OutputIndex::try_from(index).expect("small");
                if in_set(script) {
                    set.insert((tx.id, index), (*value, script.clone()));
                    delta.outputs_added += 1;
                    delta.zatoshis_added += value;
                    xor(&mut delta, digest(tx.id, index, *value, script));
                    created = true;
                }
            }
            if created {
                delta.transactions_added += 1;
                assert_eq!(holders(&set), before + 1);
            }
            for &(prev, index) in &tx.inputs {
                let before = holders(&set);
                if let Some((value, script)) = set.remove(&(prev, index)) {
                    delta.outputs_removed += 1;
                    delta.zatoshis_removed += value;
                    xor(&mut delta, digest(prev, index, value, &script));
                    delta.transactions_removed += before - holders(&set);
                }
            }
        }
        deltas.push(delta);
    }
    deltas
}

#[test]
fn engine_builds_the_utxo_set_summary() {
    let chain: Vec<Vec<Tx>> = vec![
        // h0: coinbase A makes two set members around one output outside the set.
        vec![Tx {
            id: 0xa0,
            inputs: vec![COINBASE],
            outputs: vec![(50, p2pkh(1)), (0, op_return()), (30, p2sh(2))],
        }],
        // h1: B spends A:0. A still holds A:2.
        vec![Tx {
            id: 0xb0,
            inputs: vec![(0xa0, 0)],
            outputs: vec![(49, p2pkh(3))],
        }],
        // h2 (next batch): C spends A:2, emptying A across batches, and A:1,
        // which was never in the set. D spends C:0 in the same block.
        vec![
            Tx {
                id: 0xc0,
                inputs: vec![(0xa0, 2), (0xa0, 1)],
                outputs: vec![(29, p2pkh(4))],
            },
            Tx {
                id: 0xd0,
                inputs: vec![(0xc0, 0)],
                outputs: vec![],
            },
        ],
        // h3: E spends B:0 and makes two members.
        vec![Tx {
            id: 0xe0,
            inputs: vec![(0xb0, 0)],
            outputs: vec![(10, p2pkh(5)), (5, p2pkh(6))],
        }],
        // h4 and h5 share a batch: F spends E:0, then G spends E:1. Extracting
        // h4 already sees h5's spend of the sibling, and must still leave the
        // emptying to h5.
        vec![Tx {
            id: 0xf0,
            inputs: vec![(0xe0, 0)],
            outputs: vec![(9, p2sh(7))],
        }],
        vec![Tx {
            id: 0xf1,
            inputs: vec![(0xe0, 1)],
            outputs: vec![(4, p2pkh(8)), (1, p2pkh(9))],
        }],
        // h6: H spends both of G's outputs in one transaction.
        vec![Tx {
            id: 0xf2,
            inputs: vec![(0xf1, 0), (0xf1, 1)],
            outputs: vec![(5, p2pkh(10))],
        }],
    ];
    let expected = model(&chain);

    let backend = InMemoryBackend::new();
    let pipelines = IndexPipelines::new()
        .with::<TxidLocationIndex>()
        .with::<TransparentDataIndex>()
        .with::<TransparentSpendsIndex>()
        .with::<TxOutSetIndex>();
    let mut engine = SyncEngine::from_pipelines(
        pipelines,
        backend.clone(),
        EngineConfig {
            batch_size: 2,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("engine builds");
    let blocks = chain
        .iter()
        .enumerate()
        .map(|(height, txs)| block(u64::try_from(height).expect("small"), txs))
        .collect();
    engine.sync_range(blocks).expect("sync succeeds");

    let reader = backend.reader().expect("reader");
    let stored: Vec<TxOutSetDelta> = reader
        .scan(Namespace::new("txout_set"))
        .expect("scan txout_set")
        .into_iter()
        .map(|(_, value)| decode_value::<TxOutSetIndex>(&value).expect("value decodes"))
        .collect();
    assert_eq!(stored, expected, "each block's delta matches the model");

    // The fold is the summary. Left unspent: F:0 (9) and H:0 (5).
    let totals = stored
        .into_iter()
        .try_fold(TxOutSetDelta::EMPTY, TxOutSetDelta::combine)
        .expect("fold stays in range")
        .totals()
        .expect("totals stay in range");
    assert_eq!(totals.transactions, 2);
    assert_eq!(totals.tx_outs, 2);
    assert_eq!(totals.total_zatoshis, 14);
    assert_eq!(totals.bytes_serialized, 2 * ENTRY_LEN);
    let mut unspent = digest(0xf0, 0, 9, &p2sh(7));
    for (byte, other) in unspent.iter_mut().zip(digest(0xf2, 0, 5, &p2pkh(10))) {
        *byte ^= other;
    }
    assert_eq!(totals.hash_serialized, unspent);
}
