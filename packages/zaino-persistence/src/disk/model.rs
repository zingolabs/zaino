//! Test schema + [`Model`] (every table's contents, as plain collections) shared by `tests.rs` and
//! `model_tests.rs`
//!
//! - compared as bytes, printed decoded (`owner 3 seq 17`, `id 9`, `17 on branch 1`)

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    ops::Range,
    path::Path,
    sync::Arc,
};

use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

use super::*;
use crate::{
    fs::SimFs,
    lsm::file_name,
    port::MapView,
    schema::{MapTable, SequenceTable, Tables, Width},
};

pub(super) const ROOT: &str = "/idx";

pub(super) const BLOCKS: SequenceTable = SequenceTable::new(0, "blocks", Width::Variable);
pub(super) const HEIGHTS: SequenceTable = SequenceTable::new(1, "heights", Width::fixed(8));
pub(super) const NODES: SequenceTable = SequenceTable::new(2, "pool/nodes", Width::fixed(4));
pub(super) const SCANNED: MapTable = MapTable::scoped(0, "scanned", 12, 8, 8).deletes();
pub(super) const PROBED: MapTable = MapTable::point(1, "probed", 16, 4);

const SEQUENCES: [SequenceTable; 3] = [BLOCKS, HEIGHTS, NODES];

/// Every shape an index declares: variable sequence, fixed one, one in a sub-directory, scoped map
/// with removals (read per owner), insert-only point-lookup map
pub(super) const TABLES: Tables = Tables::new(&SEQUENCES, &[SCANNED, PROBED]);

pub(super) const SCHEMA: Schema = Schema::new("blocks", 1, "regtest", TABLES);

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

/// `flush_device` = ext4 / XFS-like (commits take the `Barrier` path)
pub(super) fn sim_fs(flush_device: bool) -> Arc<SimFs> {
    let fs = SimFs::new();
    if flush_device {
        fs.flush_device();
    }
    fs
}

pub(super) fn block_at(height: u32) -> BlockRef {
    block(height, 0)
}

/// Hash `[height; 32]`, first byte ^ `branch`
fn block(height: u32, branch: u8) -> BlockRef {
    let byte = u8::try_from(height).expect("test chains stay under 256 blocks (one hash byte)");
    let mut hash = [byte; 32];
    hash[0] ^= branch;
    BlockRef { hash: BlockHash(hash), height: BlockHeight::from(height) }
}

/// `blocks` record at `position`: 0 to 22 bytes (empty records included)
pub(super) fn block_record(position: usize, branch: u8) -> Vec<u8> {
    let [low, ..] = position.to_le_bytes();
    vec![low ^ branch; position % 23]
}

/// Golden-ratio multiplier: distinct inputs → uniform leading key bytes (the filter shards on them)
const SPREAD: u64 = 0x9e37_79b9_7f4a_7c15;

fn hex(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    bytes.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
}

/// `scanned` key: `spread(owner) ‖ seq`, ordered as the store orders it (bytes)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ScannedKey([u8; 12]);

impl ScannedKey {
    pub(super) fn new(owner: u8, seq: u32) -> Self {
        let mut key = [0; 12];
        key[..8].copy_from_slice(&u64::from(owner).wrapping_mul(SPREAD).to_be_bytes());
        key[8..].copy_from_slice(&seq.to_be_bytes());
        Self(key)
    }

    fn seq(&self) -> u32 {
        let [.., a, b, c, d] = self.0;
        u32::from_be_bytes([a, b, c, d])
    }
}

impl AsRef<[u8]> for ScannedKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// `owner 3 seq 17`, hex when no owner builds it
impl fmt::Debug for ScannedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seq = self.seq();
        match (0..=u8::MAX).find(|&owner| Self::new(owner, seq) == *self) {
            Some(owner) => write!(f, "owner {owner} seq {seq}"),
            None => hex(&self.0, f),
        }
    }
}

/// `probed` key: `spread(n) ‖ 0000 ‖ n` (hash-like, distinct per `n`)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ProbedId([u8; 16]);

impl ProbedId {
    pub(super) fn new(n: u32) -> Self {
        let mut id = [0; 16];
        id[..8].copy_from_slice(&u64::from(n).wrapping_mul(SPREAD).to_be_bytes());
        id[12..].copy_from_slice(&n.to_be_bytes());
        Self(id)
    }
}

impl AsRef<[u8]> for ProbedId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// `id 9`, hex when no `n` builds it
impl fmt::Debug for ProbedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [.., a, b, c, d] = self.0;
        let n = u32::from_be_bytes([a, b, c, d]);
        match Self::new(n) == *self {
            true => write!(f, "id {n}"),
            false => hex(&self.0, f),
        }
    }
}

/// Map value or fixed-width record: `branch ‖ n` big-endian (a reorg's rewrite != what it dropped)
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Stamp(Bytes);

impl Stamp {
    fn new(n: u64, branch: u8, width: NonZeroUsize) -> Self {
        let n_bytes = n.to_be_bytes();
        let (dropped, kept) = n_bytes.split_at(n_bytes.len() + 1 - width.get());
        assert!(dropped.iter().all(|&byte| byte == 0), "{n} overflows a {width}-byte stamp");
        Self(Bytes::from([&[branch], kept].concat()))
    }
}

/// `17`, `17 on branch 1`
impl fmt::Debug for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some((&branch, n_bytes)) = self.0.split_first() else { return write!(f, "empty") };
        let n = n_bytes.iter().fold(0u64, |n, &byte| n << 8 | u64::from(byte));
        match branch {
            0 => write!(f, "{n}"),
            branch => write!(f, "{n} on branch {branch}"),
        }
    }
}

/// Key bytes kept as answered (one no owner builds prints as hex)
fn scanned_rows(answer: Vec<(Bytes, Bytes)>) -> Vec<(ScannedKey, Stamp)> {
    let row = |(key, value): (Bytes, Bytes)| {
        let key = <[u8; 12]>::try_from(&key[..]).unwrap_or_else(|_| {
            panic!("scanned answered a {}-byte key, width 12: {key:?}", key.len())
        });
        (ScannedKey(key), Stamp(value))
    };
    answer.into_iter().map(row).collect()
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

/// `asked` through `values` (one batch) and `value` (one at a time): each answer = its pair's
fn assert_lookups<K: AsRef<[u8]> + fmt::Debug + PartialEq>(
    map: MapView<'_, impl MapRead>,
    asked: &[(K, Option<Stamp>)],
    what: &str,
) {
    let keys: Vec<&[u8]> = asked.iter().map(|(key, _)| key.as_ref()).collect();
    let paired = |answers: Vec<Option<Bytes>>| -> Vec<(&K, Option<Stamp>)> {
        let answers = answers.into_iter().map(|answer| answer.map(Stamp));
        asked.iter().map(|(key, _)| key).zip(answers).collect()
    };
    let want: Vec<(&K, Option<Stamp>)> =
        asked.iter().map(|(key, value)| (key, value.clone())).collect();
    assert_same(&paired(map.values(&keys)), &want, &format!("{what}, one batch"));
    let singly = keys.iter().map(|key| map.value(key)).collect();
    assert_same(&paired(singly), &want, &format!("{what}, one at a time"));
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
/// - `branch` = stamped into every item the next advance writes (0 outside the model test)
/// - `bytes` = inserted item bytes (lower bound on what a buffer holds: removals uncounted)
#[derive(Debug, Clone, Default)]
pub(super) struct Model {
    pub(super) tip: Option<BlockRef>,
    pub(super) branch: u8,
    pub(super) bytes: usize,
    pub(super) last_seq: u32,
    ids_issued: u32,
    sequences: [Vec<Bytes>; SEQUENCES.len()],
    scanned: BTreeMap<ScannedKey, Stamp>,
    removed: BTreeSet<ScannedKey>,
    probed: BTreeMap<ProbedId, Stamp>,
}

impl Model {
    /// Durable blocks from genesis (`tip` height + 1)
    pub(super) fn blocks(&self) -> u32 {
        self.tip.map_or(0, |tip| u32::from(tip.height) + 1)
    }

    /// Next block, applied here, returned as `BlockChanges`
    pub(super) fn advance(&mut self, writes: &Writes) -> BlockChanges {
        let tip = block(self.blocks(), self.branch);
        self.tip = Some(tip);
        let mut changes = BlockChanges::new(tip, SCHEMA);

        let removed_seqs: BTreeSet<u32> = writes.removed.iter().copied().collect();
        let removed: Vec<ScannedKey> =
            self.scanned.keys().filter(|key| removed_seqs.contains(&key.seq())).copied().collect();
        for key in removed {
            changes.map(SCANNED).remove(key.as_ref());
            self.scanned.remove(&key);
            self.removed.insert(key);
        }

        for _ in 0..writes.records {
            self.append(&mut changes, BLOCKS);
            self.append(&mut changes, NODES);
            self.append(&mut changes, NODES);
        }
        self.append(&mut changes, HEIGHTS);

        for &owner in &writes.owners {
            self.last_seq += 1;
            let key = ScannedKey::new(owner, self.last_seq);
            let value = Stamp::new(u64::from(self.last_seq), self.branch, SCANNED.value);
            changes.map(SCANNED).insert(key.as_ref(), &value.0);
            self.bytes += key.0.len() + value.0.len();
            self.scanned.insert(key, value);
        }

        for _ in 0..writes.ids {
            let id = ProbedId::new(self.ids_issued);
            let value = Stamp::new(u64::from(self.ids_issued), self.branch, PROBED.value);
            self.ids_issued += 1;
            changes.map(PROBED).insert(id.as_ref(), &value.0);
            self.bytes += id.0.len() + value.0.len();
            self.probed.insert(id, value);
        }
        changes
    }

    /// Next record of `table` (from its position + `branch`), here and in `changes`
    fn append(&mut self, changes: &mut BlockChanges, table: SequenceTable) {
        let held = &mut self.sequences[usize::from(table.id)];
        let position = held.len();
        let record = match table.record {
            Width::Variable => Bytes::from(block_record(position, self.branch)),
            Width::Fixed(width) => {
                let position = u64::try_from(position).expect("usize fits u64");
                Stamp::new(position, self.branch, width).0
            }
        };
        changes.sequence(table).append(&record);
        self.bytes += record.len();
        held.push(record);
    }

    /// `scanned` rows in `range`
    fn scan(&self, range: Range<ScannedKey>) -> Vec<(ScannedKey, Stamp)> {
        // BTreeMap::range panics on start > end
        if range.is_empty() {
            return Vec::new();
        }
        self.scanned.range(range).map(|(key, value)| (*key, value.clone())).collect()
    }

    pub(super) fn assert_view(&self, view: &(impl SequenceRead + MapRead), label: &str) {
        assert_eq!(view.tip(), self.tip, "{label}: tip");
        self.assert_sequences(view, label);
        self.assert_scanned(view.map(SCANNED), label);
        self.assert_probed(view.map(PROBED), label);
    }

    /// Each sequence: one record at a time (+ one past the end), all at once, middle third
    fn assert_sequences(&self, view: &impl SequenceRead, label: &str) {
        for (table, held) in SEQUENCES.iter().zip(&self.sequences) {
            let (name, read) = (table.name, view.sequence(*table));
            let len = read.len();
            let one_at_a_time: Vec<Bytes> = (0..=len).map_while(|at| read.record(at)).collect();
            assert_same(&one_at_a_time, held, &format!("{label}: {name}, one record at a time"));
            assert_same(&read.records(0..len), held, &format!("{label}: {name} 0..{len}"));
            let middle = len / 3..len - len / 3;
            let held_middle: Vec<Bytes> = (0..)
                .zip(held)
                .filter(|(at, _)| middle.contains(at))
                .map(|(_, r)| r.clone())
                .collect();
            let what = format!("{label}: {name} {middle:?}");
            assert_same(&read.records(middle), &held_middle, &what);
        }
    }

    /// Whole map + per-owner scans, then lookups: every removed key, every 3rd held one, a never
    /// issued one
    fn assert_scanned(&self, scanned: MapView<'_, impl MapRead>, label: &str) {
        let every = ScannedKey([0; 12])..ScannedKey([0xff; 12]);
        let owners =
            [0u8, 3, 9].map(|owner| ScannedKey::new(owner, 0)..ScannedKey::new(owner, u32::MAX));
        for range in [every].into_iter().chain(owners) {
            let answer = scanned.range(range.start.as_ref(), range.end.as_ref(), usize::MAX);
            let answer = scanned_rows(answer.expect("unbounded"));
            assert_same(&answer, &self.scan(range.clone()), &format!("{label}: scanned {range:?}"));
        }

        let removed = self.removed.iter().map(|key| (*key, None));
        let held = self.scanned.iter().step_by(3).map(|(key, value)| (*key, Some(value.clone())));
        let never_issued = (ScannedKey::new(1, self.last_seq + 1), None);
        let asked: Vec<_> = removed.chain(held).chain([never_issued]).collect();
        assert_lookups(scanned, &asked, &format!("{label}: scanned removed + held + never issued"));
    }

    /// Every id issued + as many never issued, each asked twice in a row (answers in caller order)
    fn assert_probed(&self, probed: MapView<'_, impl MapRead>, label: &str) {
        let asked: Vec<(ProbedId, Option<Stamp>)> = (0..2 * self.ids_issued + 2)
            .map(ProbedId::new)
            .flat_map(|id| [id, id])
            .map(|id| (id, self.probed.get(&id).cloned()))
            .collect();
        assert_lookups(probed, &asked, &format!("{label}: probed issued + never issued"));
    }

    /// `range` at every limit around the answer's size + `limit`: over the limit = `None`, never a
    /// truncated answer
    pub(super) fn assert_scans(
        &self,
        view: &impl MapRead,
        range: Range<ScannedKey>,
        limit: u16,
        label: &str,
    ) {
        let held = self.scan(range.clone());
        let len = held.len();
        for limit in [len.saturating_sub(1), len, len + 1, usize::from(limit)] {
            let answer = view.map(SCANNED).range(range.start.as_ref(), range.end.as_ref(), limit);
            let want = (len <= limit).then(|| held.clone());
            let what = format!("{label}: {range:?} at most {limit} (holds {len})");
            match (answer.map(scanned_rows), want) {
                (Some(answer), Some(want)) => assert_same(&answer, &want, &what),
                (None, None) => {}
                (answer, want) => panic!(
                    "{what}: answered {:?} rows, want {:?} (None = over the limit)",
                    answer.map(|answer| answer.len()),
                    want.map(|want| want.len())
                ),
            }
        }
    }
}

/// - each map's deepest tier under `stall_at` segments (commits wait there)
/// - `just_opened`: every listed segment + sums on disk, beyond them only running merges' outputs
pub(super) fn assert_tiers(store: &DiskStore, fs: &SimFs, just_opened: bool, label: &str) {
    for (table, log) in SCHEMA.maps().iter().zip(&store.maps) {
        let name = table.name;
        let per_tier = log.segments_per_tier();
        let deepest = per_tier.values().max().copied().unwrap_or(0);
        let stall_at = lsm::stall_at(log.fanout());
        assert!(
            deepest < stall_at,
            "{label}: {name} holds {deepest} segments in one tier, want < {stall_at}: segments per \
             tier {per_tier:?}"
        );
        if just_opened {
            let on_disk: BTreeSet<String> =
                fs.list(&Path::new(ROOT).join(name)).expect("list").into_iter().collect();
            let listed: BTreeSet<String> = (log.segments().iter())
                .map(|segment| file_name(segment.id))
                .flat_map(|file| [format!("{file}.crc"), file])
                .collect();
            let lost: Vec<_> = listed.difference(&on_disk).collect();
            assert!(lost.is_empty(), "{label}: {name} lost listed {lost:?}: on disk {on_disk:?}");
            let unlisted: Vec<_> = on_disk.difference(&listed).collect();
            let merging = log.merging();
            assert!(
                unlisted.len() <= 2 * merging,
                "{label}: {name} holds unlisted {unlisted:?}, want <= 2 per running merge \
                 ({merging})"
            );
        }
    }
}
