//! Deferral of `Scattered` namespaces' writes during a bulk load.
//!
//! During the initial catch-up, the `Scattered` (hash-keyed) indexes land each
//! batch on scattered B-tree leaves, turning every insert into a copy-on-write
//! page rewrite. These are `address_history`, `transparent_spends` and
//! `txid_location` — the three heavy indexes this exists for — and also
//! `hash_to_height`, which is `Scattered` because its key is the block hash (low
//! volume, one entry per block, but scattered all the same). Any namespace the
//! index set declares `Scattered` is deferrable; the backend never special-cases
//! names. Deferral routes those puts to a per-namespace sorted run log ([`log`])
//! instead: each commit appends one fsynced segment and records the committed
//! length in the manifest ([`manifest`]) *in the same LMDB transaction as the
//! watermark*. [`finish_bulk`](merge::finish_bulk) then k-way merges the segments
//! and loads them with a single ordered `APPEND` pass.
//!
//! This module owns the shared, interior-mutable bulk state and the per-commit
//! routing; [`log`] owns the segment format, [`manifest`] the durable record, and
//! [`merge`] the resumable merge. `lib.rs` stays the `Backend`/`Writer`/`Reader`
//! glue and drives this module from [`LmdbWriter::commit`](crate::LmdbWriter) and
//! the [`Backend`](zaino_persistence::Backend) bulk methods.
//!
//! # Routing rule
//!
//! A `Scattered` namespace's put is deferred to its run log when **either** the
//! namespace already has a run log (a manifest entry — pending from this run or a
//! previous one) **or** bulk mode is active with deferral enabled. Otherwise the
//! put goes straight to the tree, exactly as today. The manifest-entry arm makes
//! the decision per namespace and independent of the current policy, so a store
//! left pending by a crash keeps routing that namespace to its log even under a
//! disabled policy — a split between log and tree, which `finish_bulk`'s
//! append-into-empty-tree pass cannot reconcile, can never form. `WalkOrdered`
//! and `Meta` namespaces are never deferred.
//!
//! # Disk budget
//!
//! The run logs hold the raw entries of the deferred namespaces (≈ 40–50 GB on
//! mainnet today, the three indexes' final size minus B-tree slack). During
//! `finish_bulk` both a log and its growing tree exist; peak extra disk ≈ the log
//! size, released as each namespace finishes and its log is deleted. The logs
//! live under `<lmdb path>/deferred/`.

pub(crate) mod log;
pub(crate) mod manifest;
pub(crate) mod merge;

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::Mutex;

use lmdb::{Environment, Transaction};

use zaino_persistence::{
    BulkPolicy, CommitError, KeyOrder, Namespace, OpenError, RawKey, RawValue, WriteOp,
};

use crate::open_error;
use manifest::{RunEntry, MANIFEST_NAMESPACE};

/// The shared deferral controller: one per backend, cloned by reference
/// (`Arc`) into every writer and reader handle.
///
/// Reads of completeness go straight to the manifest in LMDB (see
/// [`crate::LmdbReader::is_complete`]); only the mutable routing state — whether
/// bulk mode is active and the open run logs — lives here behind a [`Mutex`].
pub(crate) struct Deferral {
    /// `<lmdb path>/deferred/`, where run logs live.
    dir: PathBuf,
    /// Mutable bulk state, guarded for the writer/`begin_bulk`/`finish_bulk`
    /// callers (all serialised against one LMDB writer in practice).
    state: Mutex<State>,
}

/// The mutable bulk state behind [`Deferral`]'s mutex.
struct State {
    /// Whether [`Deferral::begin_bulk`] has been called without a completing
    /// [`merge::finish_bulk`] since — i.e. a bulk load is in progress.
    active: bool,
    /// The policy's `enabled` flag from the most recent `begin_bulk`.
    enabled: bool,
    /// Open run logs, keyed by namespace: every namespace with a manifest entry,
    /// loaded on open and as puts are deferred.
    runs: HashMap<Namespace, RunLog>,
}

/// One namespace's open run log and its committed extent.
///
/// The log's path is not stored: it is derived from the namespace on demand
/// ([`Deferral::log_path`]), the one place it is needed (the merge).
struct RunLog {
    /// Append handle. Writes always land at the current end of file; after a
    /// truncation on reopen, that end is the committed length.
    file: File,
    /// Committed segment count (matches the manifest run entry).
    segment_count: u64,
    /// Committed byte length (matches the manifest run entry). Bytes past it on
    /// disk belong to an uncommitted batch and are truncated on reopen.
    len: u64,
}

/// The outcome of [`Deferral::prepare`]: the ops to apply in the LMDB transaction
/// and the segment appends to finalise once it commits.
///
/// The manifest run-entry puts are folded into `direct` (they target the reserved
/// [`MANIFEST_NAMESPACE`], a `Meta` namespace), so the commit's transaction loop
/// writes them in the same transaction as the watermark with no special case.
pub(crate) struct Prepared {
    /// Ops to apply in the transaction: the batch's `WalkOrdered`/`Meta`/direct
    /// `Scattered` ops, plus one manifest run-entry put per deferred namespace.
    pub(crate) direct: Vec<WriteOp>,
    /// Segment appends already fsynced to the run logs, to commit or roll back
    /// once the transaction's fate is known. Opaque to `lib.rs`, which only moves
    /// it back into [`Deferral::finalize_committed`] / [`Deferral::finalize_aborted`].
    pub(crate) actions: Vec<SegmentAction>,
}

/// A fsynced segment append awaiting the transaction's outcome.
///
/// Crate-visible only so it can appear in [`Prepared`]; `lib.rs` moves it back
/// into the finalize methods without inspecting it.
pub(crate) enum SegmentAction {
    /// An append to a namespace whose run log already existed.
    Existing {
        /// The namespace appended to.
        ns: Namespace,
        /// The log length before this append — the rollback target.
        old_len: u64,
        /// The log length after this append.
        new_len: u64,
        /// The segment count after this append.
        new_segments: u64,
    },
    /// The first append to a namespace, which created its run log.
    New {
        /// The namespace whose run log was created.
        ns: Namespace,
        /// The newly opened append handle, inserted into [`State::runs`] on commit.
        file: File,
        /// The run log path, for removal on rollback.
        path: PathBuf,
        /// The log length after this (first) append.
        new_len: u64,
    },
}

impl SegmentAction {
    /// The manifest run entry this append commits to: the post-append segment
    /// count and log length.
    fn manifest_entry(&self) -> RunEntry {
        match *self {
            SegmentAction::Existing {
                new_len,
                new_segments,
                ..
            } => RunEntry {
                segment_count: new_segments,
                log_len: new_len,
            },
            SegmentAction::New { new_len, .. } => RunEntry {
                segment_count: 1,
                log_len: new_len,
            },
        }
    }
}

impl Deferral {
    /// Open the deferral controller, loading any pending run logs from the
    /// manifest and truncating each to its committed length.
    ///
    /// Bytes on a run log past its manifest `log_len` belong to a batch whose
    /// watermark never committed (a crash between the log fsync and the
    /// transaction commit), so they are truncated here before the batch replays.
    /// A log shorter than its manifest length is impossible for a clean store and
    /// is reported as corruption.
    pub(crate) fn open(
        env: &Environment,
        dbs: &HashMap<Namespace, lmdb::Database>,
        dir: PathBuf,
    ) -> Result<Self, OpenError> {
        let meta_db = *dbs
            .get(&MANIFEST_NAMESPACE)
            .ok_or_else(|| open_error_static("manifest namespace missing"))?;

        let mut runs = HashMap::new();
        {
            let txn = env
                .begin_ro_txn()
                .map_err(|e| open_error("open manifest txn", e))?;
            let mut cursor = txn
                .open_ro_cursor(meta_db)
                .map_err(|e| open_error("open manifest cursor", e))?;
            let pending: Vec<(Namespace, RunEntry)> = {
                use lmdb::Cursor;
                cursor
                    .iter()
                    .filter(|(key, _)| manifest::is_run_key(key))
                    .map(|(key, value)| decode_pending(dbs, key, value))
                    .collect::<Result<_, _>>()?
            };
            drop(cursor);
            drop(txn);

            for (ns, entry) in pending {
                let path = log_path(&dir, ns);
                let file = open_and_truncate(&path, entry.log_len)?;
                runs.insert(
                    ns,
                    RunLog {
                        file,
                        segment_count: entry.segment_count,
                        len: entry.log_len,
                    },
                );
            }
        }

        // Reap any run log with no manifest entry. Such a log is either a stray
        // left by a crash between `finish_bulk`'s final manifest-clear and its
        // `remove_file`, or the orphaned first segment of a namespace whose first
        // deferred commit crashed before the manifest entry committed. Neither is
        // referenced by a committed watermark, so removing it is safe — and it
        // keeps the first-segment-crash case from appending the replay *after* the
        // orphan when the batch replays and recreates the log.
        reap_orphan_logs(&dir, &runs)?;

        Ok(Self {
            dir,
            state: Mutex::new(State {
                active: false,
                enabled: false,
                runs,
            }),
        })
    }

    /// Enter bulk mode with `policy`.
    ///
    /// Idempotent in effect: it only records the policy and marks the load active.
    /// Run logs pending from a previous run are already loaded (by [`open`](Self::open)),
    /// so this re-enters bulk mode on them rather than starting fresh. With
    /// `enabled == false`, no *new* namespace starts deferring, but a namespace
    /// already pending keeps routing to its log (see the module routing rule).
    pub(crate) fn begin_bulk(&self, policy: BulkPolicy) -> Result<(), CommitError> {
        let mut state = self.state.lock().expect("deferral state mutex poisoned");
        state.active = true;
        state.enabled = policy.enabled;
        Ok(())
    }

    /// Partition `ops` for a commit: direct ops (applied in the transaction) and
    /// segment appends (fsynced here, finalised after the transaction commits).
    ///
    /// For each deferred namespace this sorts the batch's pairs, encodes one
    /// segment, appends it to the run log and fsyncs, then adds the matching
    /// manifest run-entry put to `direct`. A `Delete` on a deferred namespace is a
    /// contract violation ([`CommitError::DeferredNamespaceDelete`]).
    pub(crate) fn prepare(
        &self,
        key_orders: &HashMap<Namespace, KeyOrder>,
        ops: Vec<WriteOp>,
    ) -> Result<Prepared, CommitError> {
        let mut state = self.state.lock().expect("deferral state mutex poisoned");

        let mut direct = Vec::new();
        // Grouped per namespace; within each, a `BTreeMap` sorts the batch's keys
        // bytewise (and collapses a repeated key to its last value) for the
        // segment. Namespace order across the group does not matter — each log is
        // independent and all manifest puts land in one transaction.
        let mut deferred: HashMap<Namespace, BTreeMap<RawKey, RawValue>> = HashMap::new();
        for op in ops {
            match op {
                WriteOp::Put {
                    namespace,
                    key,
                    value,
                } if is_deferred(namespace, &state, key_orders) => {
                    deferred.entry(namespace).or_default().insert(key, value);
                }
                WriteOp::Delete { namespace, key }
                    if is_deferred(namespace, &state, key_orders) =>
                {
                    let _ = key;
                    return Err(CommitError::DeferredNamespaceDelete {
                        namespace: namespace.to_string(),
                    });
                }
                direct_op => direct.push(direct_op),
            }
        }

        let mut actions = Vec::new();
        for (ns, entries) in deferred {
            let segment = log::encode_segment(&entries).map_err(|e| CommitError::WriteFailed {
                operation: "encode deferred segment",
                source: Box::new(e),
            })?;
            // Infallible on a 64-bit target (usize <= u64); the expect names the
            // platform invariant rather than reaching for an `as` cast.
            let segment_len = u64::try_from(segment.len()).expect("segment length fits u64");

            // The segment is appended and fsynced *before* the transaction, so a
            // crash after this point leaves bytes the manifest (committed in the
            // transaction) either acknowledges or, via reopen truncation, discards.
            let action = if let Some(run) = state.runs.get_mut(&ns) {
                let old_len = run.len;
                append_segment(&mut run.file, &segment)?;
                // A run log overflowing u64 bytes (16 EiB) is not representable on
                // disk; the expect names that invariant.
                let new_len = old_len
                    .checked_add(segment_len)
                    .expect("run log length fits u64");
                SegmentAction::Existing {
                    ns,
                    old_len,
                    new_len,
                    new_segments: run.segment_count + 1,
                }
            } else {
                let path = log_path(&self.dir, ns);
                let mut file = create_run_log(&self.dir, &path)?;
                append_segment(&mut file, &segment)?;
                SegmentAction::New {
                    ns,
                    file,
                    path,
                    new_len: segment_len,
                }
            };

            direct.push(WriteOp::Put {
                namespace: MANIFEST_NAMESPACE,
                key: manifest::run_key(ns),
                value: action.manifest_entry().encode(),
            });
            actions.push(action);
        }

        Ok(Prepared { direct, actions })
    }

    /// Apply the segment appends to the in-memory state after the transaction
    /// committed: existing logs advance their committed extent, new logs are
    /// inserted into [`State::runs`].
    pub(crate) fn finalize_committed(&self, actions: Vec<SegmentAction>) {
        let mut state = self.state.lock().expect("deferral state mutex poisoned");
        for action in actions {
            match action {
                SegmentAction::Existing {
                    ns,
                    new_len,
                    new_segments,
                    ..
                } => {
                    if let Some(run) = state.runs.get_mut(&ns) {
                        run.len = new_len;
                        run.segment_count = new_segments;
                    }
                }
                SegmentAction::New {
                    ns,
                    file,
                    new_len,
                    path: _,
                } => {
                    state.runs.insert(
                        ns,
                        RunLog {
                            file,
                            segment_count: 1,
                            len: new_len,
                        },
                    );
                }
            }
        }
    }

    /// Roll back the segment appends after the transaction failed: existing logs
    /// truncate back to their committed length, new logs' files are removed.
    ///
    /// This keeps the on-disk logs consistent with the uncommitted manifest even
    /// without a restart, mirroring the reopen truncation. A commit failure is
    /// otherwise fatal (the watermark did not advance), so the batch replays.
    pub(crate) fn finalize_aborted(&self, actions: Vec<SegmentAction>) {
        let mut state = self.state.lock().expect("deferral state mutex poisoned");
        for action in actions {
            match action {
                SegmentAction::Existing { ns, old_len, .. } => {
                    if let Some(run) = state.runs.get_mut(&ns) {
                        // Best effort: on failure the batch replays from the
                        // watermark, and reopen would truncate regardless.
                        let _ = run.file.set_len(old_len);
                    }
                }
                SegmentAction::New { path, file, .. } => {
                    drop(file);
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    /// A snapshot of the deferred namespaces and their committed segment count and
    /// length, for [`merge::finish_bulk`].
    fn pending(&self) -> Vec<(Namespace, u64, u64)> {
        let state = self.state.lock().expect("deferral state mutex poisoned");
        state
            .runs
            .iter()
            .map(|(ns, run)| (*ns, run.segment_count, run.len))
            .collect()
    }

    /// Mark a namespace complete once its run log is merged in: drop its open log
    /// and clear bulk mode when none remain.
    fn complete_namespace(&self, ns: Namespace) {
        let mut state = self.state.lock().expect("deferral state mutex poisoned");
        state.runs.remove(&ns);
        if state.runs.is_empty() {
            state.active = false;
        }
    }

    /// The run log path for a namespace (for [`merge`] to open read handles).
    fn log_path(&self, ns: Namespace) -> PathBuf {
        log_path(&self.dir, ns)
    }
}

/// Whether a namespace's writes are deferred right now: it already has a run log,
/// or bulk mode is active, deferral enabled, and the namespace is `Scattered`.
fn is_deferred(ns: Namespace, state: &State, key_orders: &HashMap<Namespace, KeyOrder>) -> bool {
    if state.runs.contains_key(&ns) {
        return true;
    }
    state.active && state.enabled && matches!(key_orders.get(&ns), Some(KeyOrder::Scattered))
}

/// `<dir>/<ns>.log`.
fn log_path(dir: &std::path::Path, ns: Namespace) -> PathBuf {
    dir.join(format!("{}.log", ns.as_str()))
}

/// Decode a manifest run entry and resolve its namespace against the backend's
/// declared namespaces.
fn decode_pending(
    dbs: &HashMap<Namespace, lmdb::Database>,
    key: &[u8],
    value: &[u8],
) -> Result<(Namespace, RunEntry), OpenError> {
    let ns = dbs
        .keys()
        .find(|ns| ns.as_str().as_bytes() == key)
        .copied()
        .ok_or_else(|| open_error_static("manifest names an unknown namespace"))?;
    let entry =
        RunEntry::decode(value).ok_or_else(|| open_error_static("manifest run entry malformed"))?;
    Ok((ns, entry))
}

/// Open an existing run log for appends and truncate any uncommitted tail past
/// `committed_len`.
fn open_and_truncate(path: &std::path::Path, committed_len: u64) -> Result<File, OpenError> {
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .map_err(|e| open_error("open run log", e))?;
    let actual = file
        .metadata()
        .map_err(|e| open_error("stat run log", e))?
        .len();
    if actual < committed_len {
        return Err(open_error_static(
            "run log shorter than its committed length (corrupt manifest or log)",
        ));
    }
    if actual > committed_len {
        file.set_len(committed_len)
            .map_err(|e| open_error("truncate run log", e))?;
    }
    Ok(file)
}

/// Create a namespace's run log (and the `deferred/` directory), opened for
/// appends, with the directory entries made durable.
///
/// `file.sync_data()` on a log persists its data and inode but not the directory
/// entry that names it, so a crash could leave the manifest (made durable at the
/// next flush) pointing at a log whose dirent was lost — and reopen would fail to
/// find it. To close that, after creating `deferred/` its parent (the env dir) is
/// fsynced, and after creating the log file `deferred/` is fsynced, both before
/// the manifest transaction that first references the log. This runs once per
/// namespace (only on its first deferred commit), off the per-batch hot path.
fn create_run_log(dir: &std::path::Path, path: &std::path::Path) -> Result<File, CommitError> {
    std::fs::create_dir_all(dir).map_err(|e| CommitError::WriteFailed {
        operation: "create deferred directory",
        source: Box::new(e),
    })?;
    if let Some(parent) = dir.parent() {
        fsync_dir(parent, "fsync deferred parent directory")?;
    }
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .map_err(|e| CommitError::WriteFailed {
            operation: "create run log",
            source: Box::new(e),
        })?;
    fsync_dir(dir, "fsync deferred directory")?;
    Ok(file)
}

/// Fsync a directory so entries created in it (a new subdirectory or file) are
/// durable. Opening a directory read-only and `sync_all`-ing it is the portable
/// way to flush its dirents on Unix.
fn fsync_dir(dir: &std::path::Path, operation: &'static str) -> Result<(), CommitError> {
    let handle = File::open(dir).map_err(|e| CommitError::WriteFailed {
        operation,
        source: Box::new(e),
    })?;
    handle.sync_all().map_err(|e| CommitError::WriteFailed {
        operation,
        source: Box::new(e),
    })
}

/// Delete every `*.log` in `dir` whose namespace is not in `runs` (has no
/// manifest run entry). Absent directory → nothing to do.
fn reap_orphan_logs(
    dir: &std::path::Path,
    runs: &HashMap<Namespace, RunLog>,
) -> Result<(), OpenError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(open_error("read deferred directory", e)),
    };
    for entry in entries {
        let path = entry
            .map_err(|e| open_error("read deferred directory entry", e))?
            .path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("log") {
            continue;
        }
        let names_pending = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| runs.keys().any(|ns| ns.as_str() == stem));
        if !names_pending {
            // Best effort: a failed delete only wastes disk, as before.
            let _ = std::fs::remove_file(&path);
            #[cfg(feature = "sync-profile")]
            tracing::warn!(path = %path.display(), "reaped orphan deferred run log");
        }
    }
    Ok(())
}

/// Append a segment to a run log and fsync it.
fn append_segment(file: &mut File, segment: &[u8]) -> Result<(), CommitError> {
    use std::io::Write;
    file.write_all(segment)
        .map_err(|e| CommitError::WriteFailed {
            operation: "append deferred segment",
            source: Box::new(e),
        })?;
    file.sync_data().map_err(|e| CommitError::WriteFailed {
        operation: "fsync deferred log",
        source: Box::new(e),
    })
}

/// An [`OpenError`] for a deferral corruption invariant that has no backend cause
/// to preserve — a typed `io::Error` of kind [`InvalidData`](std::io::ErrorKind::InvalidData)
/// carries the description.
fn open_error_static(message: &'static str) -> OpenError {
    open_error(
        "deferral open",
        std::io::Error::new(std::io::ErrorKind::InvalidData, message),
    )
}

#[cfg(test)]
pub(crate) mod fault {
    //! Test-only fault injection, modelling a crash at a precise point in the
    //! commit or merge sequence. Armed per thread (tests run the driving calls on
    //! their own thread); the hook makes the operation return as if the process
    //! died at that point, leaving the durable state frozen for a reopen to
    //! recover.

    use std::cell::Cell;

    thread_local! {
        /// When set, [`LmdbWriter::commit`](crate::LmdbWriter) returns right after
        /// the run-log segments are fsynced and before the transaction commits —
        /// the crash window the reopen truncation must recover from.
        static STOP_AFTER_FSYNC: Cell<bool> = const { Cell::new(false) };
        /// When `Some(k)`, [`merge::finish_bulk`](super::merge::finish_bulk)
        /// returns right after committing the `k`-th chunk transaction of a
        /// namespace, modelling a crash mid-merge.
        static STOP_AFTER_CHUNK: Cell<Option<u64>> = const { Cell::new(None) };
        /// When `Some(n)`, the merge commits every `n` entries instead of the
        /// production chunk size, so a test can force many chunks over a tiny
        /// dataset.
        static CHUNK_SIZE: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Arm a simulated crash after the next commit's segment fsync.
    pub(crate) fn arm_stop_after_fsync() {
        STOP_AFTER_FSYNC.with(|flag| flag.set(true));
    }

    /// Consume the fsync fault flag, returning whether it was armed.
    pub(crate) fn take_stop_after_fsync() -> bool {
        STOP_AFTER_FSYNC.with(|flag| flag.replace(false))
    }

    /// Arm a simulated crash after the `k`-th chunk of the current `finish_bulk`.
    pub(crate) fn arm_stop_after_chunk(k: u64) {
        STOP_AFTER_CHUNK.with(|flag| flag.set(Some(k)));
    }

    /// Whether the merge should stop after committing its `committed`-th chunk.
    pub(crate) fn should_stop_after_chunk(committed: u64) -> bool {
        STOP_AFTER_CHUNK.with(|flag| match flag.get() {
            Some(k) if committed >= k => {
                flag.set(None);
                true
            }
            _ => false,
        })
    }

    /// Override the merge chunk size for the current thread.
    pub(crate) fn set_chunk_size(n: usize) {
        CHUNK_SIZE.with(|flag| flag.set(Some(n)));
    }

    /// The merge chunk size override, if one is armed.
    pub(crate) fn chunk_size_override() -> Option<usize> {
        CHUNK_SIZE.with(Cell::get)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the deferral integration tests, used by both this
    //! module's and [`merge`](super::merge)'s test sections: one of each
    //! [`KeyOrder`], and helpers to open the backend and build ops.

    use std::path::Path;

    use zaino_persistence::{KeyOrder, Namespace, NamespaceSpec, RawValue, WriteOp};

    use crate::{LmdbBackend, LmdbConfig};

    /// A walk-ordered namespace (height-led, append-only).
    pub(crate) const WALK: Namespace = Namespace::new("headers");
    /// A scattered namespace (hash-led), deferred under an enabled bulk policy.
    pub(crate) const SCAT: Namespace = Namespace::new("address_history");
    /// A second scattered namespace, to exercise more than one deferred log.
    pub(crate) const SCAT2: Namespace = Namespace::new("txid_location");
    /// A reserved meta namespace (never deferred).
    pub(crate) const META: Namespace = Namespace::new("_watermark");

    /// The namespace set every deferral test opens with.
    pub(crate) fn specs() -> Vec<NamespaceSpec> {
        vec![
            NamespaceSpec {
                namespace: WALK,
                key_order: KeyOrder::WalkOrdered,
            },
            NamespaceSpec {
                namespace: SCAT,
                key_order: KeyOrder::Scattered,
            },
            NamespaceSpec {
                namespace: SCAT2,
                key_order: KeyOrder::Scattered,
            },
            NamespaceSpec::meta(META),
        ]
    }

    /// Open (or reopen) the backend at `dir` with the standard namespaces.
    pub(crate) fn open(dir: &Path) -> LmdbBackend {
        LmdbBackend::open(LmdbConfig {
            path: dir.to_path_buf(),
            map_size_bytes: 8 << 20,
            namespaces: specs(),
        })
        .expect("open lmdb backend")
    }

    /// The run-log path for a namespace under `dir`.
    pub(crate) fn log_path(dir: &Path, ns: Namespace) -> std::path::PathBuf {
        super::log_path(&dir.join("deferred"), ns)
    }

    /// A `Put` op.
    pub(crate) fn put(namespace: Namespace, key: RawValue, value: &[u8]) -> WriteOp {
        WriteOp::Put {
            namespace,
            key,
            value: value.to_vec(),
        }
    }

    /// A big-endian height key.
    pub(crate) fn height(h: u32) -> RawValue {
        h.to_be_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{height, log_path, open, put, META, SCAT, SCAT2, WALK};
    use super::*;
    use zaino_persistence::{Backend, BackendReader, BackendWriter, RawKey, RawValue, WriteOp};

    /// Scan a namespace to a sorted vec of owned pairs.
    fn scan(backend: &crate::LmdbBackend, ns: Namespace) -> Vec<(RawKey, RawValue)> {
        backend.reader().expect("reader").scan(ns).expect("scan")
    }

    #[test]
    fn is_complete_tracks_deferral_across_bulk_and_finish() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = open(tmp.path());
        backend
            .begin_bulk(BulkPolicy { enabled: true })
            .expect("begin_bulk");

        let mut writer = backend.writer().expect("writer");
        writer
            .commit(vec![
                put(WALK, height(0), b"h0"),
                put(SCAT, vec![0x11], b"a"),
                put(META, b"watermark".to_vec(), &1u64.to_be_bytes()),
            ])
            .expect("commit");
        drop(writer);

        let reader = backend.reader().expect("reader");
        assert!(
            !reader.is_complete(SCAT).expect("is_complete"),
            "a deferred scattered namespace reads incomplete after a bulk commit"
        );
        assert!(
            reader.is_complete(WALK).expect("is_complete"),
            "a walk-ordered namespace stays complete in bulk mode"
        );
        assert!(
            reader.is_complete(META).expect("is_complete"),
            "a meta namespace stays complete in bulk mode"
        );
        assert!(
            reader.is_complete(SCAT2).expect("is_complete"),
            "a scattered namespace with no deferred writes stays complete"
        );
        drop(reader);

        backend.finish_bulk().expect("finish_bulk");
        let reader = backend.reader().expect("reader");
        for ns in [WALK, SCAT, SCAT2, META] {
            assert!(
                reader.is_complete(ns).expect("is_complete"),
                "{ns} is complete after finish_bulk"
            );
        }
    }

    #[test]
    fn delete_on_deferred_namespace_is_a_typed_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = open(tmp.path());
        backend
            .begin_bulk(BulkPolicy { enabled: true })
            .expect("begin_bulk");

        let mut writer = backend.writer().expect("writer");
        let err = writer
            .commit(vec![WriteOp::Delete {
                namespace: SCAT,
                key: vec![0x11],
            }])
            .expect_err("a delete on a deferred namespace is rejected");
        assert!(
            matches!(&err, CommitError::DeferredNamespaceDelete { namespace } if namespace == SCAT.as_str()),
            "expected DeferredNamespaceDelete naming {SCAT}, got {err:?}"
        );
    }

    #[test]
    fn delete_outside_bulk_mode_is_allowed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = open(tmp.path());
        // No begin_bulk: the scattered namespace is not deferred, so a delete is
        // the ordinary direct path (today's behaviour).
        let mut writer = backend.writer().expect("writer");
        writer
            .commit(vec![put(SCAT, vec![0x11], b"a")])
            .expect("put");
        writer
            .commit(vec![WriteOp::Delete {
                namespace: SCAT,
                key: vec![0x11],
            }])
            .expect("a delete outside bulk mode succeeds");
        assert!(scan(&backend, SCAT).is_empty());
    }

    #[test]
    fn crash_after_fsync_truncates_orphan_and_replay_has_no_duplicates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = log_path(tmp.path(), SCAT);

        let batch_one = || {
            vec![
                put(WALK, height(0), b"h0"),
                put(SCAT, vec![0x11], b"a1"),
                put(META, b"watermark".to_vec(), &1u64.to_be_bytes()),
            ]
        };
        let batch_two = || {
            vec![
                put(WALK, height(1), b"h1"),
                put(SCAT, vec![0x22], b"a2"),
                put(META, b"watermark".to_vec(), &2u64.to_be_bytes()),
            ]
        };

        // Batch one commits cleanly under bulk mode.
        {
            let backend = open(tmp.path());
            backend
                .begin_bulk(BulkPolicy { enabled: true })
                .expect("begin_bulk");
            let mut writer = backend.writer().expect("writer");
            writer.commit(batch_one()).expect("commit batch one");
            drop(writer);
            backend.flush().expect("flush");
        }
        let committed_len = std::fs::metadata(&log).expect("stat log").len();
        assert!(committed_len > 0, "batch one wrote a segment");

        // Batch two: crash after the segment fsync, before the transaction.
        {
            let backend = open(tmp.path());
            backend
                .begin_bulk(BulkPolicy { enabled: true })
                .expect("begin_bulk");
            let mut writer = backend.writer().expect("writer");
            fault::arm_stop_after_fsync();
            writer
                .commit(batch_two())
                .expect("commit returns at the fault");
            assert!(
                std::fs::metadata(&log).expect("stat log").len() > committed_len,
                "the fsynced-but-uncommitted segment is on disk before reopen"
            );
            drop(writer);
            drop(backend);
        }

        // Reopen truncates the orphaned segment back to the committed length.
        let backend = open(tmp.path());
        assert_eq!(
            std::fs::metadata(&log).expect("stat log").len(),
            committed_len,
            "reopen truncates the uncommitted segment"
        );

        // Replay batch two (as the engine would from the watermark) and finish.
        backend
            .begin_bulk(BulkPolicy { enabled: true })
            .expect("re-enter bulk");
        let mut writer = backend.writer().expect("writer");
        writer.commit(batch_two()).expect("replay batch two");
        drop(writer);
        backend.finish_bulk().expect("finish_bulk");

        assert_eq!(
            scan(&backend, SCAT),
            vec![(vec![0x11], b"a1".to_vec()), (vec![0x22], b"a2".to_vec())],
            "the replayed build holds each key exactly once"
        );
    }

    #[test]
    fn fsync_dir_syncs_a_directory_and_errors_on_a_missing_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        super::fsync_dir(tmp.path(), "test fsync").expect("fsync of a real directory succeeds");
        assert!(
            super::fsync_dir(&tmp.path().join("absent"), "test fsync").is_err(),
            "fsync of a missing directory is a typed error, not a panic"
        );
    }

    #[test]
    fn reopen_reaps_orphan_logs_but_keeps_pending_ones() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // A genuine pending log: a deferred commit leaves a committed manifest
        // entry for SCAT.
        {
            let backend = open(tmp.path());
            backend
                .begin_bulk(BulkPolicy { enabled: true })
                .expect("begin_bulk");
            let mut writer = backend.writer().expect("writer");
            writer
                .commit(vec![put(SCAT, vec![0x11], b"a")])
                .expect("commit");
            drop(writer);
            backend.flush().expect("flush");
        }
        let pending_log = log_path(tmp.path(), SCAT);
        assert!(pending_log.exists(), "the deferred commit wrote a run log");

        // A stray log with no manifest entry, as a crash between finish_bulk's
        // final manifest-clear and remove_file would leave.
        let orphan_log = log_path(tmp.path(), SCAT2);
        std::fs::write(&orphan_log, b"orphan").expect("write stray log");

        // Reopen reaps the orphan, keeps the pending one, and the pending
        // namespace still completes.
        let backend = open(tmp.path());
        assert!(
            !orphan_log.exists(),
            "a run log with no manifest entry is reaped on reopen"
        );
        assert!(
            pending_log.exists(),
            "the pending run log is kept on reopen"
        );
        backend.finish_bulk().expect("finish_bulk");
        assert_eq!(scan(&backend, SCAT), vec![(vec![0x11], b"a".to_vec())]);
    }
}
