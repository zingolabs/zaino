//! The measured bodies, each mirroring one way the indexer or the store uses
//! storage.
//!
//! Every body is generic over [`BenchTarget`], so the same instructions run
//! against each backend and the only difference between two results is the
//! implementation underneath.
//!
//! | body | what it mirrors |
//! |---|---|
//! | [`commit_batches`] | the sync engine's batch write: every index's ops for a batch of blocks, plus the watermark, in one atomic commit, flushed once at the end |
//! | [`read_by_height`] | a served read that already knows the height — the common case |
//! | [`read_by_hash`] | the extra hash→height lookup a read by hash pays first |
//! | [`compose_block`] | the store's compose-on-read: the seven-namespace fan-out one served compact block needs |
//! | [`scan_namespace`] | the whole-namespace walk state loading does, and the place a ranged `range` read would slot in |
//! | [`verify`] | a read-back equality check, so a fast-but-wrong backend cannot win |

use zaino_persistence::{Backend, BackendReader, BackendWriter, Namespace, WriteOp};
use zaino_primitives::types::Height;

use crate::data::{self, Generator, Shape};
use crate::error::BenchError;
use crate::target::BenchTarget;

/// Pre-generated write operations, grouped into the batches the engine commits.
///
/// Built outside the measured section: generating records is the benchmark's own
/// cost, not the backend's.
pub struct Batches {
    /// One entry per batch, each the ops of every index for that batch's blocks
    /// plus the watermark the batch stamps.
    batches: Vec<Vec<WriteOp>>,
    /// Key and value bytes across every op — the logical volume, before any
    /// backend's own overhead.
    logical_bytes: u64,
    /// The height keys written, in height order, for the read bodies.
    height_keys: Vec<Vec<u8>>,
    /// The hash keys written, in the same order.
    hash_keys: Vec<Vec<u8>>,
    /// Each block's header entry, for [`verify`].
    header_entries: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Batches {
    /// Generate `blocks` blocks of `shape` from `seed`, grouped into batches of
    /// `batch_size` — the engine's default batch is 1000 blocks, so a benchmark
    /// that wants to reflect production keeps that shape even at a smaller
    /// block count.
    pub fn generate(
        shape: Shape,
        seed: u64,
        blocks: u32,
        batch_size: u32,
    ) -> Result<Self, BenchError> {
        let mut generator = Generator::new(shape, seed);
        let mut batches = Vec::new();
        // The first batch stamps every namespace's format, as the indexer does.
        let mut current = data::format_stamps();
        let mut logical_bytes = 0u64;
        let mut height_keys = Vec::with_capacity(usize::try_from(blocks).unwrap_or_default());
        let mut hash_keys = Vec::with_capacity(usize::try_from(blocks).unwrap_or_default());
        let mut header_entries = Vec::with_capacity(usize::try_from(blocks).unwrap_or_default());

        for offset in 0..blocks {
            let height =
                Height::try_from(offset).map_err(|e| BenchError::Generate(e.to_string()))?;
            let records = generator.block(height)?;

            logical_bytes = logical_bytes.saturating_add(records.bytes);
            height_keys.push(records.height_key);
            hash_keys.push(records.hash_key);
            header_entries.push(records.header_entry);
            current.extend(records.ops);

            let closes_batch = (offset + 1) % batch_size == 0 || offset + 1 == blocks;
            if closes_batch {
                // The watermark rides in the same batch as the entries it
                // vouches for, exactly as the engine stamps it.
                current.push(data::watermark_op(height));
                batches.push(std::mem::take(&mut current));
            }
        }

        Ok(Self {
            batches,
            logical_bytes,
            height_keys,
            hash_keys,
            header_entries,
        })
    }

    /// Logical key plus value bytes across every op.
    pub fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    /// How many blocks were generated.
    pub fn blocks(&self) -> usize {
        self.height_keys.len()
    }
}

/// Commit every batch and flush once — the engine's write path.
///
/// The flush is inside the measured section on purpose: a backend that defers
/// durability would otherwise be credited for work it has not finished.
pub fn commit_batches<T: BenchTarget>(target: &T, batches: Batches) -> Result<u64, BenchError> {
    let mut writer = target.backend().writer()?;
    let mut committed = 0u64;
    for batch in batches.batches {
        committed = committed.saturating_add(u64::try_from(batch.len()).unwrap_or_default());
        writer.commit(batch)?;
    }
    target.backend().flush()?;
    Ok(committed)
}

/// A target already holding `blocks` blocks, with the keys to address them.
pub struct Populated<T: BenchTarget> {
    target: T,
    height_keys: Vec<Vec<u8>>,
    hash_keys: Vec<Vec<u8>>,
    header_entries: Vec<(Vec<u8>, Vec<u8>)>,
    logical_bytes: u64,
}

impl<T: BenchTarget> Populated<T> {
    /// Stand up a fresh target and fill it. Setup only — never timed.
    pub fn build(
        shape: Shape,
        seed: u64,
        blocks: u32,
        batch_size: u32,
    ) -> Result<Self, BenchError> {
        let batches = Batches::generate(shape, seed, blocks, batch_size)?;
        let height_keys = batches.height_keys.clone();
        let hash_keys = batches.hash_keys.clone();
        let header_entries = batches.header_entries.clone();
        let logical_bytes = batches.logical_bytes;

        let target = T::create()?;
        commit_batches(&target, batches)?;

        Ok(Self {
            target,
            height_keys,
            hash_keys,
            header_entries,
            logical_bytes,
        })
    }

    /// The populated backend, for the workloads that take their own readers.
    pub fn backend(&self) -> &T::Backend {
        self.target.backend()
    }

    /// A reader over the populated store.
    pub fn reader(&self) -> Result<<T::Backend as Backend>::Reader, BenchError> {
        Ok(self.target.backend().reader()?)
    }

    /// The height key of block `index`, wrapping so a caller can walk freely.
    pub fn height_key(&self, index: usize) -> &[u8] {
        let len = self.height_keys.len().max(1);
        self.height_keys
            .get(index % len)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The hash key of block `index`, wrapping likewise.
    pub fn hash_key(&self, index: usize) -> &[u8] {
        let len = self.hash_keys.len().max(1);
        self.hash_keys
            .get(index % len)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// How many blocks it holds.
    pub fn blocks(&self) -> usize {
        self.height_keys.len()
    }

    /// Logical bytes written into it.
    pub fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    /// Bytes it occupies on disk.
    pub fn on_disk_bytes(&self) -> u64 {
        self.target.on_disk_bytes()
    }

    /// Read back every block's header entry and compare it to what was written.
    ///
    /// The correctness gate: a backend that loses or mangles a value cannot post
    /// a fast number and pass.
    pub fn verify(&self) -> Result<usize, BenchError> {
        let reader = self.reader()?;
        let namespace: Namespace = zaino_indexes::indexes::headers::ID.into();
        for (key, value) in &self.header_entries {
            match reader.get(namespace, key)? {
                None => {
                    return Err(BenchError::Absent {
                        namespace: namespace.to_string(),
                    });
                }
                Some(found) if &found != value => {
                    return Err(BenchError::Mismatch {
                        namespace: namespace.to_string(),
                    });
                }
                Some(_) => {}
            }
        }
        Ok(self.header_entries.len())
    }
}

/// One point read by height key, from the namespace a served read hits first.
pub fn read_by_height<R: BackendReader>(reader: &R, key: &[u8]) -> Result<usize, BenchError> {
    Ok(reader
        .get(zaino_indexes::indexes::headers::ID.into(), key)?
        .map(|value| value.len())
        .unwrap_or_default())
}

/// One point read by hash key — the hash→height lookup, whose key is a 32-byte
/// hash rather than a height, so it probes a different key distribution.
pub fn read_by_hash<R: BackendReader>(reader: &R, key: &[u8]) -> Result<usize, BenchError> {
    Ok(reader
        .get(data::hash_namespace(), key)?
        .map(|value| value.len())
        .unwrap_or_default())
}

/// The store's compose-on-read fan-out: every namespace one served compact block
/// is assembled from, at one height.
pub fn compose_block<R: BackendReader>(reader: &R, height_key: &[u8]) -> Result<usize, BenchError> {
    let mut bytes = 0usize;
    for namespace in data::block_namespaces() {
        bytes = bytes.saturating_add(
            reader
                .get(namespace, height_key)?
                .map(|value| value.len())
                .unwrap_or_default(),
        );
    }
    Ok(bytes)
}

/// [`compose_block`] from `threads` threads at once, each composing `per_thread`
/// blocks, every compose through a fresh reader as the store takes one per block
/// read. Returns the wall time of the whole run: the number that moves when
/// readers contend on something shared (a lock, a cache line), which a
/// single-threaded loop cannot see.
pub fn compose_concurrent<T>(
    store: &Populated<T>,
    threads: usize,
    per_thread: usize,
) -> Result<std::time::Duration, BenchError>
where
    T: BenchTarget + Sync,
{
    let started = std::time::Instant::now();
    std::thread::scope(|scope| -> Result<(), BenchError> {
        let workers: Vec<_> = (0..threads)
            .map(|thread| {
                scope.spawn(move || -> Result<(), BenchError> {
                    for step in 0..per_thread {
                        let reader = store.backend().reader()?;
                        let index = thread.wrapping_mul(7919).wrapping_add(step);
                        std::hint::black_box(compose_block(&reader, store.height_key(index))?);
                    }
                    Ok(())
                })
            })
            .collect();
        for worker in workers {
            worker
                .join()
                .map_err(|_| BenchError::Generate("a compose thread panicked".to_owned()))??;
        }
        Ok(())
    })?;
    Ok(started.elapsed())
}

/// A whole-namespace walk — what pipeline state loading does today, and the
/// shape a ranged read would replace.
pub fn scan_namespace<R: BackendReader>(
    reader: &R,
    namespace: Namespace,
) -> Result<usize, BenchError> {
    Ok(reader.scan(namespace)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{InMemory, Lmdb};

    #[test]
    fn batching_stamps_one_watermark_per_batch_and_covers_every_block() -> Result<(), BenchError> {
        // 10 blocks in batches of 4 → batches of 4, 4 and 2.
        let batches = Batches::generate(Shape::Light, 1, 10, 4)?;

        assert_eq!(batches.batches.len(), 3);
        assert_eq!(batches.blocks(), 10);
        let watermarks = batches
            .batches
            .iter()
            .flatten()
            .filter(|op| match op {
                WriteOp::Put { namespace, .. } | WriteOp::Delete { namespace, .. } => {
                    namespace.as_str() == "_watermark"
                }
            })
            .count();
        assert_eq!(watermarks, 3, "one watermark per batch");
        assert!(batches.logical_bytes() > 0);
        Ok(())
    }

    /// The point of the whole exercise: identical instructions, either backend.
    #[test]
    fn both_backends_populate_verify_and_serve_the_same_reads() -> Result<(), BenchError> {
        fn exercise<T: BenchTarget>() -> Result<(), BenchError> {
            let populated = Populated::<T>::build(Shape::Typical, 42, 8, 4)?;
            assert_eq!(populated.verify()?, 8, "{}", T::NAME);

            let reader = populated.reader()?;
            assert!(
                read_by_height(&reader, populated.height_key(0))? > 0,
                "height"
            );
            assert!(read_by_hash(&reader, populated.hash_key(3))? > 0, "hash");
            assert!(
                compose_block(&reader, populated.height_key(5))? > 0,
                "compose"
            );
            assert_eq!(
                scan_namespace(&reader, zaino_indexes::indexes::headers::ID.into())?,
                8,
                "scan sees every header"
            );
            Ok(())
        }

        exercise::<InMemory>()?;
        exercise::<Lmdb>()
    }

    /// Whatever the backend stores, the composed answer must be the same bytes —
    /// otherwise the two columns of a report are not measuring one workload.
    #[test]
    fn the_two_backends_agree_on_every_composed_byte_count() -> Result<(), BenchError> {
        let lmdb = Populated::<Lmdb>::build(Shape::Typical, 11, 6, 3)?;
        let memory = Populated::<InMemory>::build(Shape::Typical, 11, 6, 3)?;

        let (lmdb_reader, memory_reader) = (lmdb.reader()?, memory.reader()?);
        for index in 0..6 {
            assert_eq!(
                compose_block(&lmdb_reader, lmdb.height_key(index))?,
                compose_block(&memory_reader, memory.height_key(index))?,
                "block {index}"
            );
        }
        assert_eq!(lmdb.logical_bytes(), memory.logical_bytes());
        Ok(())
    }

    #[test]
    fn a_verify_failure_is_reported_rather_than_passing_silently() -> Result<(), BenchError> {
        let populated = Populated::<InMemory>::build(Shape::Light, 3, 4, 2)?;
        // Overwrite one header value behind the benchmark's back.
        let mut writer = populated.target.backend().writer()?;
        writer.commit(vec![WriteOp::Put {
            namespace: zaino_indexes::indexes::headers::ID.into(),
            key: populated.height_key(0).to_vec(),
            value: vec![0u8; 72],
        }])?;

        assert!(matches!(
            populated.verify(),
            Err(BenchError::Mismatch { .. })
        ));
        Ok(())
    }
}
