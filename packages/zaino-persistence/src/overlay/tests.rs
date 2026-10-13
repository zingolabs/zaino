use zaino_primitives::types::BlockHash;
use zcash_protocol::consensus::NetworkType;

use super::*;
use crate::{
    manifest::IndexKind,
    port::{MapTable, SequenceTable, Tables, Width},
};

const RECORDS: SequenceTable = SequenceTable::new(0, "records", Width::Variable);
const IDS: MapTable = MapTable::new(0, "ids", Width::fixed(16), Width::fixed(4), 0).deletes();
const SCHEMA: Schema =
    Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, Tables::new(&[RECORDS], &[IDS]));
const OTHER: Schema =
    Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, Tables::new(&[], &[]));

/// Height `n`, hash `[n; 32]`
fn block_ref(n: u8) -> BlockRef {
    let height = Height::try_from(u32::from(n)).expect("small height");
    BlockRef { hash: BlockHash::from([n; 32]), height }
}

/// Block `n`: one record + one id, both `n`
fn block(n: u8) -> BlockChanges {
    let mut changes = BlockChanges::new(block_ref(n), SCHEMA);
    changes.sequence(RECORDS).append(&[n]);
    changes.map(IDS).insert(&[n; 16], &[n; 4]);
    changes
}

impl Overlay {
    /// Per-table items = what the blocks added; tips ascending
    pub(crate) fn check(&self, label: &str) {
        let tips: Vec<_> = self.deltas.iter().map(|delta| delta.tip.height).collect();
        assert!(tips.is_sorted_by(|a, b| a < b), "{label}: layer blocks ascending {tips:?}");
        let records = appended(self.deltas.iter(), self.sequences.len());
        for (at, (held, records)) in self.sequences.iter().zip(records).enumerate() {
            assert_eq!(held.len(), records, "{label}: sequence {at} = its blocks' appends");
        }
        for (at, held) in self.maps.iter().enumerate() {
            let mut keys: Vec<&Bytes> =
                self.deltas.iter().flat_map(|delta| &delta.keys[at]).collect();
            keys.sort_unstable();
            keys.dedup();
            let same = held.len() == keys.len() && keys.iter().all(|key| held.contains_key(*key));
            assert!(same, "{label}: map {at} = its blocks' keys");
            let owned = held.iter().all(|(key, (owner, _))| {
                let wrote = self.deltas.iter().filter(|delta| delta.keys[at].contains(key));
                wrote.map(|delta| delta.tip.height).max() == Some(*owner)
            });
            assert!(owned, "{label}: map {at} each entry owned by the newest block writing it");
        }
    }
}

/// Committed side of an `OverlayView`: a tip + a schema, nothing else read
#[derive(Clone)]
struct Durable {
    tip: Option<BlockRef>,
    schema: Schema,
}

impl View for Durable {
    fn tip(&self) -> Option<BlockRef> {
        self.tip
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }
}

#[test]
#[should_panic(expected = "apply at height 1, not above the last applied Some(Height(1))")]
fn with_a_block_not_above_the_layer_panics() {
    let layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    layer.with(&block(1));
}

#[test]
#[should_panic(expected = "changes built for another schema")]
fn with_a_block_of_another_schema_panics() {
    let layer = Overlay::empty(&SCHEMA).with(&block(0));
    layer.with(&BlockChanges::new(block_ref(1), OTHER));
}

#[test]
#[should_panic(expected = "ids: a map key held twice")]
fn with_a_key_the_layer_holds_panics() {
    let layer = Overlay::empty(&SCHEMA).with(&block(0));
    let mut again = layer.changes(block_ref(1));
    again.map(IDS).insert(&[0; 16], &[9; 4]);
    layer.with(&again);
}

#[test]
#[should_panic(expected = "ids: a map key removed twice")]
fn with_a_key_the_layer_removed_panics() {
    let mut removal = BlockChanges::new(block_ref(0), SCHEMA);
    removal.map(IDS).remove(&[0; 16]);
    let layer = Overlay::empty(&SCHEMA).with(&removal);
    let mut again = layer.changes(block_ref(1));
    again.map(IDS).remove(&[0; 16]);
    layer.with(&again);
}

#[test]
fn rebase_drops_every_block_durable_holds() {
    let layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    let durable = |n| Durable { tip: Some(block_ref(n)), schema: SCHEMA };

    assert_eq!(layer.rebase(&durable(0)), Overlay::empty(&SCHEMA).with(&block(1)));
    assert_eq!(layer.rebase(&durable(1)), Overlay::empty(&SCHEMA));
}

#[test]
#[should_panic(expected = "not one of the layer's blocks")]
fn rebase_onto_another_branch_panics() {
    let layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    let other_branch = BlockRef { hash: BlockHash::from([7; 32]), ..block_ref(0) };
    layer.rebase(&Durable { tip: Some(other_branch), schema: SCHEMA });
}

#[test]
#[should_panic(expected = "not one of the layer's blocks")]
fn rebase_past_the_layer_panics() {
    let layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    layer.rebase(&Durable { tip: Some(block_ref(2)), schema: SCHEMA });
}

#[test]
#[should_panic(expected = "not above durable")]
fn a_view_over_a_layer_not_above_durable_panics() {
    let layer = Overlay::empty(&SCHEMA).with(&block(1));
    OverlayView::new(Durable { tip: Some(block_ref(1)), schema: SCHEMA }, layer);
}

#[test]
#[should_panic(expected = "layer over another schema's view")]
fn a_view_over_another_schemas_durable_panics() {
    let layer = Overlay::empty(&SCHEMA).with(&block(1));
    OverlayView::new(Durable { tip: None, schema: OTHER }, layer);
}

// `Overlay::check` invariants, each broken by hand (check never firing = silent pass after every
// `disk::tests::history` step)

#[test]
#[should_panic(expected = "layer blocks ascending")]
fn check_catches_blocks_out_of_order() {
    let mut layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    let oldest = layer.deltas.pop_front().expect("two blocks");
    layer.deltas.push_back(oldest);
    layer.check("drill");
}

#[test]
#[should_panic(expected = "sequence 0 = its blocks' appends")]
fn check_catches_a_record_no_block_appended() {
    let mut layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    layer.sequences[0].push_back(Bytes::from_static(b"stray"));
    layer.check("drill");
}

#[test]
#[should_panic(expected = "map 0 = its blocks' keys")]
fn check_catches_a_key_a_block_wrote_gone_missing() {
    let mut layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    layer.maps[0].remove(&[0u8; 16][..]);
    layer.check("drill");
}

#[test]
#[should_panic(expected = "map 0 = its blocks' keys")]
fn check_catches_a_key_no_block_wrote() {
    let mut layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    let stray = (block_ref(1).height, Entry::Value(Bytes::from_static(b"rows")));
    layer.maps[0].insert(Bytes::from(vec![0xee; 16]), stray);
    layer.check("drill");
}

/// Rebase dropping the newer block would keep its key: a removed value resurrected
#[test]
#[should_panic(expected = "map 0 each entry owned by the newest block writing it")]
fn check_catches_an_entry_owned_by_an_older_block() {
    let mut layer = Overlay::empty(&SCHEMA).with(&block(0)).with(&block(1));
    layer.maps[0].get_mut(&[1u8; 16][..]).expect("block 1 wrote its id").0 = block_ref(0).height;
    layer.check("drill");
}
