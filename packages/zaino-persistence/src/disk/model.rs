//! Test schema + `Model` (every table's contents as `Vec` / `BTreeMap`) shared by `tests.rs` and
//! `model_tests.rs`
//!
//! - keys + values print decoded (`owner 3 seq 17`, `id 9`), never as raw bytes

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
    sync::Arc,
};

use zcash_protocol::consensus::{BlockHeight, NetworkType};
use zcash_protocol::BlockHash;

use super::*;
use crate::{
    fs::SimFs,
    lsm::file_name,
    schema::{IndexKind, MapTable, SequenceTable, Tables, Width},
};

pub(super) const ROOT: &str = "/idx";

pub(super) const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable);
pub(super) const HEIGHTS: SequenceTable = SequenceTable::new(1, "heights", Width::fixed(8));
pub(super) const NODES: SequenceTable = SequenceTable::new(2, "pool/nodes", Width::fixed(4));
pub(super) const SCANNED: MapTable = MapTable::scoped(0, "scanned", 12, 8, 8).deletes();
pub(super) const PROBED: MapTable = MapTable::point(1, "probed", 16, 4);

/// Every shape an index declares: variable sequence, fixed one, one in a sub-directory, scoped map
/// with removals (`account ‖ seq`, read per account), insert-only point-lookup map (hash-like ids)
pub(super) const TABLES: Tables = Tables::new(&[BLOCKS, HEIGHTS, NODES], &[SCANNED, PROBED]);

pub(super) const SCHEMA: Schema =
    Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, TABLES);

/// Commits only when told (`write_buffer` never reached)
pub(super) fn open(engine: &DiskEngine) -> DiskStore {
    engine.open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX).expect("open")
}

impl DiskEngine {
    /// Merges after `fanout` same-tier segments (small = merges in a handful of commits)
    pub(super) fn with_fanout(fs: Arc<dyn Fs>, fanout: usize) -> Self {
        let fanout = Fanout::new(fanout).expect("fanout >= 2");
        Self::new(fs, LsmConfig { fanout, ..LsmConfig::default() })
    }
}

impl DiskStore {
    /// Blocks until every running merge finishes (lands on the next commit)
    pub(super) fn settle(&self) {
        for log in &self.maps {
            log.settle();
        }
    }
}

/// `SimFs` flushing the whole device on any `sync_data` when `flush_device` (ext4 / XFS-like:
/// commits take the `Barrier` path)
pub(super) fn sim_fs(flush_device: bool) -> Arc<SimFs> {
    with_flush(SimFs::new(), flush_device)
}

pub(super) fn with_flush(fs: Arc<SimFs>, flush_device: bool) -> Arc<SimFs> {
    if flush_device {
        fs.flush_device();
    }
    fs
}

/// Block at `height` on the first branch (hash `[height; 32]`)
pub(super) fn block_at(height: u32) -> BlockRef {
    on_branch(height, 0)
}

/// Block at `height` on branch `branch` (first hash byte ^ `branch`)
fn on_branch(height: u32, branch: u8) -> BlockRef {
    let mut hash = [height as u8; 32];
    hash[0] ^= branch;
    BlockRef { hash: BlockHash(hash), height: BlockHeight::from(height) }
}

/// Record `n` of `blocks`: 0 to 22 bytes of `n` (empty records included)
pub(super) fn block_record(n: u32) -> Vec<u8> {
    vec![n as u8; n as usize % 23]
}

/// Uniform 8 bytes per owner (the LSM shards on leading key bytes)
fn account(owner: u8) -> [u8; 8] {
    u64::from(owner).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes()
}

pub(super) fn scanned_key(owner: u8, seq: u32) -> Vec<u8> {
    [&account(owner)[..], &seq.to_be_bytes()].concat()
}

/// Uniform first 8 bytes, `n` in the last 4 (distinct per `n`)
pub(super) fn probed_key(n: u32) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    id[12..].copy_from_slice(&n.to_be_bytes());
    id
}

/// `bytes` on branch `branch` (a dropped record != its replacement)
fn on_branch_bytes(mut bytes: Vec<u8>, branch: u8) -> Vec<u8> {
    if let Some(first) = bytes.first_mut() {
        *first ^= branch;
    }
    bytes
}

/// Map key, printed decoded: `owner 3 seq 17` (`scanned`), `id 9` (`probed`), else hex
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Key(pub(super) Vec<u8>);

impl Key {
    /// `scanned` key's `(owner, seq)` (`None` = not a key [`scanned_key`] builds)
    fn scanned(&self) -> Option<(u8, u32)> {
        let (head, seq) = self.0.split_first_chunk::<8>()?;
        let seq = u32::from_be_bytes(*<&[u8; 4]>::try_from(seq).ok()?);
        let owner = (0..=u8::MAX).find(|owner| account(*owner) == *head)?;
        Some((owner, seq))
    }

    /// `probed` key's id (`None` = not a key [`probed_key`] builds)
    fn probed(&self) -> Option<u32> {
        let (_, id) = self.0.split_last_chunk::<4>()?;
        let id = u32::from_be_bytes(*id);
        (probed_key(id) == self.0).then_some(id)
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.scanned(), self.probed()) {
            (Some((owner, seq)), _) => write!(f, "owner {owner} seq {seq}"),
            (None, Some(id)) => write!(f, "id {id}"),
            (None, None) => self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}")),
        }
    }
}

/// Map value, printed decoded: big-endian number, `on branch b` when its first byte carries one
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Value(pub(super) Vec<u8>);

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some((&branch, rest)) = self.0.split_first() else { return write!(f, "empty") };
        let number = rest.iter().fold(0u64, |number, byte| number << 8 | u64::from(*byte));
        match branch {
            0 => write!(f, "{number}"),
            branch => write!(f, "{number} on branch {branch}"),
        }
    }
}

pub(super) type Row = (Key, Value);

pub(super) fn rows(rows: Vec<(Bytes, Bytes)>) -> Vec<Row> {
    rows.into_iter().map(|(key, value)| (Key(key.to_vec()), Value(value.to_vec()))).collect()
}

/// `got == want`, else the first differing position + both lengths (never a whole-vector dump)
pub(super) fn assert_same<T: PartialEq + fmt::Debug>(got: &[T], want: &[T], what: &str) {
    if got == want {
        return;
    }
    let at = got
        .iter()
        .zip(want)
        .position(|(got, want)| got != want)
        .unwrap_or(got.len().min(want.len()));
    panic!(
        "{what}: first difference at [{at}]: got {:?}, want {:?} (got {} items, want {})",
        got.get(at),
        want.get(at),
        got.len(),
        want.len()
    );
}

/// One block's writes ([`Model::advance`]): `records` blocks (+2 `pool/nodes` each), one height, a
/// fresh `scanned` row per `owners` entry, `ids` fresh `probed` ids, `removed` `scanned` seqs
/// (skipped unless held before the block)
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Writes {
    pub(super) records: u8,
    pub(super) owners: Vec<u8>,
    pub(super) ids: u16,
    pub(super) removed: Vec<u32>,
}

/// One line, vectors past 8 items summarised (a shrunk case stays under a screen)
impl fmt::Debug for Writes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn list<T: fmt::Debug>(items: &[T]) -> String {
            match items.len() {
                0..=8 => format!("{items:?}"),
                len => format!("{:?}… ({len} items)", &items[..8]),
            }
        }
        write!(
            f,
            "records {}, owners {}, ids {}, removed {}",
            self.records,
            list(&self.owners),
            self.ids,
            list(&self.removed)
        )
    }
}

/// Every table's contents through the last [`advance`](Self::advance)
///
/// - `branch` = branch of the next advance's contents (0 outside the model test)
/// - `bytes` = inserted item bytes (lower bound on what a buffer holds: removals uncounted)
#[derive(Debug, Clone, Default)]
pub(super) struct Model {
    pub(super) tip: Option<BlockRef>,
    pub(super) branch: u8,
    pub(super) bytes: usize,
    blocks: Vec<Vec<u8>>,
    heights: Vec<Vec<u8>>,
    nodes: Vec<Vec<u8>>,
    scanned: BTreeMap<Key, Value>,
    removed: BTreeSet<Key>,
    probed: BTreeMap<Key, Value>,
    pub(super) seq: u32,
    ids: u32,
}

impl Model {
    /// Durable blocks from genesis (`tip` height + 1)
    pub(super) fn blocks(&self) -> usize {
        self.tip.map_or(0, |tip| u32::from(tip.height) as usize + 1)
    }

    /// Next block, applied here, returned as `BlockChanges`
    pub(super) fn advance(&mut self, writes: &Writes) -> BlockChanges {
        let tip = on_branch(self.blocks() as u32, self.branch);
        self.tip = Some(tip);
        let mut changes = BlockChanges::new(tip, SCHEMA);
        let removed: BTreeSet<u32> = writes.removed.iter().copied().collect();
        let held: Vec<Key> = (self.scanned.keys())
            .filter(|key| {
                key.0.last_chunk().is_some_and(|seq| removed.contains(&u32::from_be_bytes(*seq)))
            })
            .cloned()
            .collect();
        for key in held {
            changes.map(SCANNED).remove(&key.0);
            self.scanned.remove(&key);
            self.removed.insert(key);
        }
        for _ in 0..writes.records {
            let record = block_record(self.blocks.len() as u32);
            self.append(&mut changes, BLOCKS, record);
            for _ in 0..2 {
                let node = (self.nodes.len() as u32).to_le_bytes().to_vec();
                self.append(&mut changes, NODES, node);
            }
        }
        let height = (self.blocks() as u64).to_be_bytes().to_vec();
        self.append(&mut changes, HEIGHTS, height);
        for &owner in &writes.owners {
            self.seq += 1;
            let key = scanned_key(owner, self.seq);
            let value = on_branch_bytes(u64::from(self.seq).to_be_bytes().to_vec(), self.branch);
            changes.map(SCANNED).insert(&key, &value);
            self.bytes += key.len() + value.len();
            self.scanned.insert(Key(key), Value(value));
        }
        for n in self.ids..self.ids + u32::from(writes.ids) {
            let value = on_branch_bytes(n.to_be_bytes().to_vec(), self.branch);
            changes.map(PROBED).insert(&probed_key(n), &value);
            self.bytes += 16 + value.len();
            self.probed.insert(Key(probed_key(n)), Value(value));
        }
        self.ids += u32::from(writes.ids);
        changes
    }

    /// `record` on this branch, at the end of `table` here and in `changes`
    fn append(&mut self, changes: &mut BlockChanges, table: SequenceTable, record: Vec<u8>) {
        let record = on_branch_bytes(record, self.branch);
        changes.sequence(table).append(&record);
        self.bytes += record.len();
        let held = if table == BLOCKS {
            &mut self.blocks
        } else if table == HEIGHTS {
            &mut self.heights
        } else {
            &mut self.nodes
        };
        held.push(record);
    }

    /// `scanned` rows in `start..end`
    pub(super) fn scan(&self, start: &[u8], end: &[u8]) -> Vec<Row> {
        let rows = self.scanned.iter().filter(|(key, _)| start <= &key.0[..] && &key.0[..] < end);
        rows.map(|(key, value)| (key.clone(), value.clone())).collect()
    }

    /// - every sequence record (one at a time + as ranges), full + per-owner scans
    /// - sampled values + a miss
    /// - values over every id issued + as many never issued (each asked twice: caller order)
    pub(super) fn assert_view(&self, view: &(impl SequenceRead + MapRead), label: &str) {
        assert_eq!(view.tip(), self.tip, "{label}: tip");
        for (table, model) in
            [(BLOCKS, &self.blocks), (HEIGHTS, &self.heights), (NODES, &self.nodes)]
        {
            let (name, read) = (table.name, view.sequence(table));
            let len = model.len() as u64;
            assert_eq!(read.len(), len, "{label}: {name} record count");
            let model: Vec<Bytes> = model.iter().cloned().map(Bytes::from).collect();
            let one_by_one: Vec<_> = (0..len).map(|at| read.record(at)).collect();
            let expected: Vec<_> = model.iter().cloned().map(Some).collect();
            assert_same(&one_by_one, &expected, &format!("{label}: {name} records one by one"));
            assert_eq!(read.record(len), None, "{label}: {name} record {len} = past the end");
            let what = format!("{label}: {name} records 0..{len}");
            assert_same(&read.records(0..len), &model, &what);
            let middle = len / 3..len - len / 3;
            let expected = &model[middle.start as usize..middle.end as usize];
            let what = format!("{label}: {name} records {middle:?}");
            assert_same(&read.records(middle), expected, &what);
        }

        let scanned = view.map(SCANNED);
        let every = self.scan(&[0; 12], &[0xff; 12]);
        let all = scanned.range(&[0; 12], &[0xff; 12], usize::MAX).expect("unbounded");
        assert_same(&rows(all), &every, &format!("{label}: scanned, every row"));
        for owner in [0u8, 3, 9] {
            let (from, to) = (scanned_key(owner, 0), scanned_key(owner, u32::MAX));
            let answer = scanned.range(&from, &to, usize::MAX).expect("unbounded");
            assert_same(&rows(answer), &self.scan(&from, &to), &format!("{label}: owner {owner}"));
        }
        for (key, value) in self.scanned.iter().step_by(7) {
            let got = scanned.value(&key.0).map(|got| Value(got.to_vec()));
            assert_eq!(got.as_ref(), Some(value), "{label}: scanned value of {key:?}");
        }
        let never = Key(scanned_key(1, self.seq + 1));
        assert_eq!(scanned.value(&never.0), None, "{label}: {never:?} never issued");
        // every removed key + a sample of held ones, as one batch and one by one
        let held = self.scanned.iter().step_by(3).map(|(key, value)| (key, Some(value)));
        let asked: Vec<(&Key, Option<&Value>)> =
            self.removed.iter().map(|key| (key, None)).chain(held).collect();
        let keys: Vec<&[u8]> = asked.iter().map(|(key, _)| key.0.as_slice()).collect();
        let want: Vec<(&Key, Option<Value>)> =
            asked.iter().map(|(key, value)| (*key, value.cloned())).collect();
        let answered = |values: Vec<Option<Bytes>>| -> Vec<(&Key, Option<Value>)> {
            let values = values.into_iter().map(|value| value.map(|value| Value(value.to_vec())));
            asked.iter().map(|(key, _)| *key).zip(values).collect()
        };
        let what = format!("{label}: scanned removed + held, one batch");
        assert_same(&answered(scanned.values(&keys)), &want, &what);
        let singles = keys.iter().map(|key| scanned.value(key)).collect();
        assert_same(&answered(singles), &want, &format!("{label}: scanned removed + held, singly"));

        let probed = view.map(PROBED);
        let asked: Vec<Key> =
            (0..2 * self.ids + 2).flat_map(|n| [Key(probed_key(n)), Key(probed_key(n))]).collect();
        let want: Vec<(&Key, Option<&Value>)> =
            asked.iter().map(|id| (id, self.probed.get(id))).collect();
        let keys: Vec<&[u8]> = asked.iter().map(|id| id.0.as_slice()).collect();
        let answered = |values: Vec<Option<Bytes>>| -> Vec<(&Key, Option<Value>)> {
            let values = values.into_iter().map(|value| value.map(|value| Value(value.to_vec())));
            asked.iter().zip(values).collect()
        };
        let want: Vec<(&Key, Option<Value>)> =
            want.into_iter().map(|(id, value)| (id, value.cloned())).collect();
        let what = format!("{label}: probed ids issued + never issued, one batch");
        assert_same(&answered(probed.values(&keys)), &want, &what);
        let singles = keys.iter().map(|id| probed.value(id)).collect();
        let what = format!("{label}: probed ids issued + never issued, singly");
        assert_same(&answered(singles), &want, &what);
    }

    /// `range` over `from..to` at every limit around the answer's size + `limit`: over the limit =
    /// `None`, never a truncated answer
    pub(super) fn assert_scans(
        &self,
        view: &impl MapRead,
        (from, to): (&Key, &Key),
        limit: u16,
        label: &str,
    ) {
        let expected = self.scan(&from.0, &to.0);
        let len = expected.len();
        for limit in [len.saturating_sub(1), len, len + 1, usize::from(limit)] {
            let answer = view.map(SCANNED).range(&from.0, &to.0, limit).map(rows);
            let bounded = (len <= limit).then(|| expected.clone());
            let what = format!("{label}: {from:?}..{to:?} at most {limit} (holds {len})");
            match (answer, bounded) {
                (Some(answer), Some(bounded)) => assert_same(&answer, &bounded, &what),
                (None, None) => {}
                (answer, bounded) => panic!(
                    "{what}: answered {:?} rows, want {:?} (None = over the limit)",
                    answer.map(|answer| answer.len()),
                    bounded.map(|bounded| bounded.len())
                ),
            }
        }
    }
}

/// - each map's tiers within `fanout` inputs + `STALL_WINDOWS` (2) idle windows
/// - `just_opened`: every listed segment + sums on disk, beyond them only running merges' outputs
pub(super) fn assert_tiers(
    store: &DiskStore,
    fs: &SimFs,
    fanout: usize,
    just_opened: bool,
    label: &str,
) {
    for (table, log) in SCHEMA.maps().iter().zip(&store.maps) {
        let name = table.name;
        let mut per_tier = BTreeMap::<u32, usize>::new();
        for segment in log.segments() {
            *per_tier.entry(segment.records.ilog(fanout as u64)).or_default() += 1;
        }
        let most = per_tier.values().max().copied().unwrap_or(0);
        assert!(
            most < 3 * fanout,
            "{label}: {name} holds {most} segments in one tier, want < 3 × fanout {fanout}: segments \
             per tier {per_tier:?}"
        );
        if just_opened {
            let on_disk = fs.list(&Path::new(ROOT).join(name)).expect("list");
            for segment in log.segments() {
                let file = file_name(segment.id);
                let missing: Vec<_> = [file.clone(), format!("{file}.crc")]
                    .into_iter()
                    .filter(|f| !on_disk.contains(f))
                    .collect();
                assert!(
                    missing.is_empty(),
                    "{label}: {name} lost listed {missing:?}: on disk {on_disk:?}"
                );
            }
            let (listed, merging) = (log.segments().len(), log.merging());
            let extra = on_disk.len() - 2 * listed;
            assert!(
                extra <= 2 * merging,
                "{label}: {name} holds {extra} unlisted files, want <= 2 per running merge \
                 ({merging}): on disk {on_disk:?}"
            );
        }
    }
}
