use std::collections::HashMap;

use zaino_proto::proto::compact_formats::CompactBlock;

/// A single field-level difference between two blocks at the same height.
#[derive(Debug, Clone)]
pub(super) struct Diff {
    pub(super) height: u64,
    pub(super) field: String,
    pub(super) value_a: String,
    pub(super) value_b: String,
}

/// Result of comparing two block sets.
#[derive(Debug)]
pub(super) struct CompareResult {
    /// Number of blocks that matched perfectly.
    pub(super) matched: u64,
    /// Field-level diffs for blocks at matching heights that differed.
    pub(super) mismatched: Vec<Diff>,
    /// Heights present in B but missing from A.
    pub(super) missing_in_a: Vec<u64>,
    /// Heights present in A but missing from B.
    pub(super) missing_in_b: Vec<u64>,
}

/// Compare two sequences of compact blocks.
///
/// Blocks are aligned by height. Both sequences are expected to be sorted by
/// height (the fetch layer guarantees this).
pub(super) fn compare_blocks(
    blocks_a: Vec<CompactBlock>,
    blocks_b: Vec<CompactBlock>,
) -> CompareResult {
    let map_a: HashMap<u64, CompactBlock> = blocks_a.into_iter().map(|b| (b.height, b)).collect();
    let map_b: HashMap<u64, CompactBlock> = blocks_b.into_iter().map(|b| (b.height, b)).collect();

    let mut matched: u64 = 0;
    let mut mismatched: Vec<Diff> = Vec::new();
    let mut missing_in_a: Vec<u64> = Vec::new();
    let mut missing_in_b: Vec<u64> = Vec::new();

    // Collect all heights from both maps.
    let mut all_heights: Vec<u64> = map_a.keys().copied().collect();
    all_heights.extend(map_b.keys().copied());
    all_heights.sort();
    all_heights.dedup();

    for height in all_heights {
        match (map_a.get(&height), map_b.get(&height)) {
            (Some(a), Some(b)) => {
                if a == b {
                    matched += 1;
                } else {
                    let diffs = diff_compact_block(height, a, b);
                    mismatched.extend(diffs);
                }
            }
            (Some(_), None) => missing_in_b.push(height),
            (None, Some(_)) => missing_in_a.push(height),
            (None, None) => unreachable!(),
        }
    }

    CompareResult {
        matched,
        mismatched,
        missing_in_a,
        missing_in_b,
    }
}

/// Produce field-level diffs for two blocks at the same height that differ.
fn diff_compact_block(height: u64, a: &CompactBlock, b: &CompactBlock) -> Vec<Diff> {
    let mut diffs = Vec::new();

    diff_field(
        &mut diffs,
        height,
        "proto_version",
        a.proto_version,
        b.proto_version,
    );
    // Height is the key — if it differs, still record it (shouldn't happen).
    diff_field(&mut diffs, height, "height", a.height, b.height);
    diff_bytes(&mut diffs, height, "hash", &a.hash, &b.hash);
    diff_bytes(&mut diffs, height, "prev_hash", &a.prev_hash, &b.prev_hash);
    diff_field(&mut diffs, height, "time", a.time, b.time);
    diff_bytes(&mut diffs, height, "header", &a.header, &b.header);

    diff_vtx(&mut diffs, height, &a.vtx, &b.vtx);
    diff_chain_metadata(&mut diffs, height, &a.chain_metadata, &b.chain_metadata);

    diffs
}

fn diff_field<T: std::fmt::Display + PartialEq>(
    diffs: &mut Vec<Diff>,
    height: u64,
    field: &str,
    a: T,
    b: T,
) {
    if a != b {
        diffs.push(Diff {
            height,
            field: field.to_string(),
            value_a: a.to_string(),
            value_b: b.to_string(),
        });
    }
}

fn diff_bytes(diffs: &mut Vec<Diff>, height: u64, field: &str, a: &[u8], b: &[u8]) {
    if a != b {
        diffs.push(Diff {
            height,
            field: field.to_string(),
            value_a: hex::encode(a),
            value_b: hex::encode(b),
        });
    }
}

fn diff_vtx(
    diffs: &mut Vec<Diff>,
    height: u64,
    vtx_a: &[zaino_proto::proto::compact_formats::CompactTx],
    vtx_b: &[zaino_proto::proto::compact_formats::CompactTx],
) {
    let count_a = vtx_a.len();
    let count_b = vtx_b.len();

    if count_a != count_b {
        diffs.push(Diff {
            height,
            field: "vtx.len".to_string(),
            value_a: count_a.to_string(),
            value_b: count_b.to_string(),
        });
    }

    let max_len = count_a.max(count_b);
    for i in 0..max_len {
        match (vtx_a.get(i), vtx_b.get(i)) {
            (Some(tx_a), Some(tx_b)) => {
                diff_field(
                    diffs,
                    height,
                    &format!("vtx[{}].index", i),
                    tx_a.index,
                    tx_b.index,
                );
                diff_bytes(
                    diffs,
                    height,
                    &format!("vtx[{}].txid", i),
                    &tx_a.txid,
                    &tx_b.txid,
                );
                diff_field(
                    diffs,
                    height,
                    &format!("vtx[{}].fee", i),
                    tx_a.fee,
                    tx_b.fee,
                );
                diff_field(
                    diffs,
                    height,
                    &format!("vtx[{}].spends.len", i),
                    tx_a.spends.len(),
                    tx_b.spends.len(),
                );
                diff_field(
                    diffs,
                    height,
                    &format!("vtx[{}].outputs.len", i),
                    tx_a.outputs.len(),
                    tx_b.outputs.len(),
                );
                diff_field(
                    diffs,
                    height,
                    &format!("vtx[{}].actions.len", i),
                    tx_a.actions.len(),
                    tx_b.actions.len(),
                );
                diff_field(
                    diffs,
                    height,
                    &format!("vtx[{}].vin.len", i),
                    tx_a.vin.len(),
                    tx_b.vin.len(),
                );
                diff_field(
                    diffs,
                    height,
                    &format!("vtx[{}].vout.len", i),
                    tx_a.vout.len(),
                    tx_b.vout.len(),
                );
            }
            (Some(_), None) => diffs.push(Diff {
                height,
                field: format!("vtx[{}]", i),
                value_a: "present".to_string(),
                value_b: "missing".to_string(),
            }),
            (None, Some(_)) => diffs.push(Diff {
                height,
                field: format!("vtx[{}]", i),
                value_a: "missing".to_string(),
                value_b: "present".to_string(),
            }),
            (None, None) => unreachable!(),
        }
    }
}

fn diff_chain_metadata(
    diffs: &mut Vec<Diff>,
    height: u64,
    a: &Option<zaino_proto::proto::compact_formats::ChainMetadata>,
    b: &Option<zaino_proto::proto::compact_formats::ChainMetadata>,
) {
    match (a, b) {
        (Some(ma), Some(mb)) => {
            diff_field(
                diffs,
                height,
                "chain_metadata.sapling_commitment_tree_size",
                ma.sapling_commitment_tree_size,
                mb.sapling_commitment_tree_size,
            );
            diff_field(
                diffs,
                height,
                "chain_metadata.orchard_commitment_tree_size",
                ma.orchard_commitment_tree_size,
                mb.orchard_commitment_tree_size,
            );
        }
        (Some(_), None) => diffs.push(Diff {
            height,
            field: "chain_metadata".to_string(),
            value_a: "present".to_string(),
            value_b: "missing".to_string(),
        }),
        (None, Some(_)) => diffs.push(Diff {
            height,
            field: "chain_metadata".to_string(),
            value_a: "missing".to_string(),
            value_b: "present".to_string(),
        }),
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_block(height: u64, hash: &[u8]) -> CompactBlock {
        CompactBlock {
            height,
            hash: hash.to_vec(),
            prev_hash: vec![0u8; 32],
            proto_version: 1,
            time: 1000,
            header: vec![],
            vtx: vec![],
            chain_metadata: None,
        }
    }

    #[test]
    fn identical_blocks_all_match() {
        let blocks_a = vec![make_block(100, &[1u8; 32]), make_block(101, &[2u8; 32])];
        let blocks_b = blocks_a.clone();

        let result = compare_blocks(blocks_a, blocks_b);
        assert_eq!(result.matched, 2);
        assert!(result.mismatched.is_empty());
        assert!(result.missing_in_a.is_empty());
        assert!(result.missing_in_b.is_empty());
    }

    #[test]
    fn different_block_hash_is_detected() {
        let blocks_a = vec![make_block(100, &[1u8; 32])];
        let blocks_b = vec![make_block(100, &[2u8; 32])];

        let result = compare_blocks(blocks_a, blocks_b);
        assert_eq!(result.matched, 0);
        assert_eq!(result.mismatched.len(), 1);
        assert_eq!(result.mismatched[0].height, 100);
        assert_eq!(result.mismatched[0].field, "hash");
    }

    #[test]
    fn missing_block_is_detected() {
        let blocks_a = vec![make_block(100, &[1u8; 32])];
        let blocks_b = vec![];

        let result = compare_blocks(blocks_a, blocks_b);
        assert_eq!(result.matched, 0);
        assert_eq!(result.missing_in_b, vec![100]);
        assert!(result.missing_in_a.is_empty());
    }

    #[test]
    fn empty_both_sides() {
        let result = compare_blocks(vec![], vec![]);
        assert_eq!(result.matched, 0);
        assert!(result.mismatched.is_empty());
        assert!(result.missing_in_a.is_empty());
        assert!(result.missing_in_b.is_empty());
    }

    #[test]
    fn same_blocks_with_different_vtx_count() {
        let block_a = make_block(100, &[1u8; 32]);
        let mut block_b = block_a.clone();

        use zaino_proto::proto::compact_formats::CompactTx;
        block_b.vtx = vec![CompactTx::default()];

        let result = compare_blocks(vec![block_a], vec![block_b]);
        assert_eq!(result.matched, 0);
        // Should have at least vtx.len diff
        let len_diff: Vec<&Diff> = result
            .mismatched
            .iter()
            .filter(|d| d.field == "vtx.len")
            .collect();
        assert_eq!(len_diff.len(), 1);
    }
}
