//! `finish_bulk`: the resumable k-way merge that loads a deferred namespace's run
//! log into its LMDB database with one ordered `APPEND` pass.
//!
//! Per deferred namespace: open a streaming [`SegmentCursor`] over each of the
//! run log's sorted segments, k-way merge them (a [`BinaryHeap`] over the cursors'
//! peeked keys — bounded memory, one cursor's small buffer per segment), and
//! `APPEND` the merged, globally sorted stream into the database in chunk
//! transactions. Each chunk records how far it loaded ([`loaded_through`]), so a
//! crash resumes by skipping keys at or below that mark. The final transaction
//! clears the namespace's manifest entries, then the log is deleted.
//!
//! [`loaded_through`]: super::manifest::loaded_through_key
//! [`SegmentCursor`]: super::log::SegmentCursor

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use lmdb::{Cursor, Database, Environment, RwTransaction, Transaction, WriteFlags};
use lmdb_sys::MDB_LAST;

use zaino_persistence::{CommitError, Namespace, RawKey, RawValue};

use super::log::{self, SegmentCursor, HEADER_LEN};
use super::manifest::{self, MANIFEST_NAMESPACE};
use super::Deferral;
use crate::commit_error;

/// Entries loaded per chunk transaction.
///
/// Each chunk is one LMDB write transaction of `APPEND`s plus the `loaded_through`
/// mark, so this trades the crash-resume granularity (a crash loses at most the
/// current chunk's progress, replayed on resume) against per-transaction
/// overhead. One million keeps transactions infrequent over the 190M+ entries of
/// `address_history` while bounding the in-memory chunk buffer and the work
/// redone after a crash.
const CHUNK_SIZE: usize = 1_000_000;

/// The chunk size to use: the production [`CHUNK_SIZE`], or a test override so a
/// test can force many chunks over a tiny dataset.
fn chunk_size() -> usize {
    #[cfg(test)]
    if let Some(n) = super::fault::chunk_size_override() {
        return n;
    }
    CHUNK_SIZE
}

/// Complete every deferred namespace: merge its run log into its database and
/// clear its manifest entries. Resumable and idempotent — a namespace with no
/// manifest entry is simply not in [`Deferral::pending`], so a second call does
/// nothing.
pub(crate) fn finish_bulk(
    deferral: &Deferral,
    env: &Environment,
    dbs: &HashMap<Namespace, Database>,
) -> Result<(), CommitError> {
    let meta_db = *dbs
        .get(&MANIFEST_NAMESPACE)
        .ok_or_else(|| CommitError::NamespaceNotFound(MANIFEST_NAMESPACE.to_string()))?;
    for (ns, segment_count, log_len) in deferral.pending() {
        let db = *dbs
            .get(&ns)
            .ok_or_else(|| CommitError::NamespaceNotFound(ns.to_string()))?;
        merge_namespace(deferral, env, db, meta_db, ns, segment_count, log_len)?;
    }
    Ok(())
}

/// Merge one namespace's run log into its database.
fn merge_namespace(
    deferral: &Deferral,
    env: &Environment,
    db: Database,
    meta_db: Database,
    ns: Namespace,
    segment_count: u64,
    log_len: u64,
) -> Result<(), CommitError> {
    let start = std::time::Instant::now();
    // A mainnet merge moves tens of GB and can run for minutes; log its start and
    // end at `info` on every build (not only under `sync-profile`), so an operator
    // sees the readiness-gating work progress. One line each, never per chunk.
    tracing::info!(
        namespace = ns.as_str(),
        segments = segment_count,
        log_bytes = log_len,
        "deferred finish_bulk: merging namespace"
    );

    let loaded_through = read_loaded_through(env, meta_db, ns)?;
    verify_target(env, db, ns, loaded_through.as_deref())?;

    let path = deferral.log_path(ns);
    let file = Arc::new(File::open(&path).map_err(|e| corrupt_io(ns, e))?);
    let mut cursors = build_cursors(&file, ns, segment_count, log_len)?;

    // Prime the heap with each segment's first key.
    let mut heap: BinaryHeap<Reverse<(RawKey, usize)>> = BinaryHeap::new();
    for (seg, cursor) in cursors.iter_mut().enumerate() {
        if let Some((key, _)) = cursor.peek().map_err(|e| corrupt(ns, e))? {
            heap.push(Reverse((key.clone(), seg)));
        }
    }

    let chunk_cap = chunk_size();
    let mut chunk: Vec<(RawKey, RawValue)> = Vec::new();
    let mut chunks_done = 0u64;
    let mut entries_loaded = 0u64;

    // `peek().map(clone)` takes an owned key and releases the heap borrow before
    // the body pops from and pushes to the heap.
    while let Some(key) = heap.peek().map(|Reverse((key, _))| key.clone()) {
        // Drain every segment holding this key; the highest-indexed (latest)
        // segment's value wins, matching plain-`put` overwrite semantics.
        let mut winner_seg: Option<usize> = None;
        let mut winner_value: Option<RawValue> = None;
        loop {
            match heap.peek() {
                Some(Reverse((k, _))) if *k == key => {}
                _ => break,
            }
            let Reverse((_, seg)) = heap.pop().expect("peek returned Some");
            let (_, value) = cursors[seg]
                .take()
                .map_err(|e| corrupt(ns, e))?
                .expect("peek promised a record for this segment");
            if winner_seg.is_none_or(|best| seg >= best) {
                winner_seg = Some(seg);
                winner_value = Some(value);
            }
            if let Some((next_key, _)) = cursors[seg].peek().map_err(|e| corrupt(ns, e))? {
                heap.push(Reverse((next_key.clone(), seg)));
            }
        }
        let value = winner_value.expect("a heap key is backed by a segment record");

        // Resume: skip anything already loaded by an earlier, interrupted finish.
        if loaded_through
            .as_deref()
            .is_some_and(|mark| key.as_slice() <= mark)
        {
            continue;
        }

        chunk.push((key, value));
        if chunk.len() >= chunk_cap {
            flush_chunk(env, db, meta_db, ns, &chunk)?;
            entries_loaded += u64::try_from(chunk.len()).expect("chunk length fits u64");
            chunk.clear();
            chunks_done += 1;
            #[cfg(test)]
            if super::fault::should_stop_after_chunk(chunks_done) {
                return Ok(());
            }
        }
    }
    if !chunk.is_empty() {
        flush_chunk(env, db, meta_db, ns, &chunk)?;
        entries_loaded += u64::try_from(chunk.len()).expect("chunk length fits u64");
        chunks_done += 1;
    }

    // One final transaction drops both manifest entries, flipping the namespace
    // to complete; only then is the log removed, so a crash before this point
    // resumes rather than losing data.
    let mut txn = env
        .begin_rw_txn()
        .map_err(|e| commit_error("begin finish txn", e))?;
    delete_if_present(&mut txn, meta_db, &manifest::run_key(ns))?;
    delete_if_present(&mut txn, meta_db, &manifest::loaded_through_key(ns))?;
    txn.commit()
        .map_err(|e| commit_error("commit finish txn", e))?;

    deferral.complete_namespace(ns);
    // Best effort: the entries are durable in the tree; a left-behind log is only
    // wasted disk and is truncated-then-ignored on the next open (no manifest
    // entry points at it).
    let _ = std::fs::remove_file(&path);

    // The always-on end line: the per-namespace entry count, chunk count and
    // wall time. This is the merge timing event the `sync-profile` build used to
    // gate; it is unconditional now because a long bulk merge is worth seeing on
    // any build, and it carries no query-level data.
    let merge_ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(
        namespace = ns.as_str(),
        merge_ms,
        entries = entries_loaded,
        chunks = chunks_done,
        "deferred finish_bulk: merged namespace"
    );
    Ok(())
}

/// Open a [`SegmentCursor`] over each segment by walking the run log's headers.
/// The segments must sum exactly to the committed length.
fn build_cursors(
    file: &Arc<File>,
    ns: Namespace,
    segment_count: u64,
    log_len: u64,
) -> Result<Vec<SegmentCursor>, CommitError> {
    let mut cursors = Vec::new();
    let mut offset = 0u64;
    for _ in 0..segment_count {
        let mut header_bytes = [0u8; HEADER_LEN];
        file.read_exact_at(&mut header_bytes, offset)
            .map_err(|e| corrupt_io(ns, e))?;
        let header = log::parse_header(&header_bytes).map_err(|e| corrupt(ns, e))?;
        cursors.push(SegmentCursor::new(Arc::clone(file), &header, offset));
        offset = offset
            .checked_add(header.total_len())
            .ok_or_else(|| corrupt_msg(ns, "run log segment offset overflow"))?;
    }
    if offset != log_len {
        return Err(corrupt_msg(
            ns,
            "run log segments do not sum to the committed length",
        ));
    }
    Ok(cursors)
}

/// `APPEND` a chunk's entries into the database and record the resume mark, all in
/// one transaction.
fn flush_chunk(
    env: &Environment,
    db: Database,
    meta_db: Database,
    ns: Namespace,
    chunk: &[(RawKey, RawValue)],
) -> Result<(), CommitError> {
    let mut txn = env
        .begin_rw_txn()
        .map_err(|e| commit_error("begin merge chunk txn", e))?;
    for (key, value) in chunk {
        txn.put(db, key, value, WriteFlags::APPEND)
            .map_err(|e| commit_error("append merged entry", e))?;
    }
    let last_key = &chunk
        .last()
        .expect("flush_chunk is called with a non-empty chunk")
        .0;
    txn.put(
        meta_db,
        &manifest::loaded_through_key(ns),
        last_key,
        WriteFlags::empty(),
    )
    .map_err(|e| commit_error("record loaded_through", e))?;
    txn.commit()
        .map_err(|e| commit_error("commit merge chunk", e))?;
    Ok(())
}

/// The resume mark recorded by a previous, interrupted finish, if any.
fn read_loaded_through(
    env: &Environment,
    meta_db: Database,
    ns: Namespace,
) -> Result<Option<RawKey>, CommitError> {
    let txn = env
        .begin_ro_txn()
        .map_err(|e| commit_error("begin loaded_through txn", e))?;
    match txn.get(meta_db, &manifest::loaded_through_key(ns)) {
        Ok(bytes) => Ok(Some(bytes.to_vec())),
        Err(lmdb::Error::NotFound) => Ok(None),
        Err(e) => Err(commit_error("read loaded_through", e)),
    }
}

/// Verify the target database is safe to `APPEND` into: empty for a fresh merge,
/// or holding only keys up to the resume mark for a resumed one. Fails loudly
/// otherwise, rather than appending over existing keys and mis-ordering the tree.
fn verify_target(
    env: &Environment,
    db: Database,
    ns: Namespace,
    loaded_through: Option<&[u8]>,
) -> Result<(), CommitError> {
    let txn = env
        .begin_ro_txn()
        .map_err(|e| commit_error("begin verify txn", e))?;
    let last = {
        let cursor = txn
            .open_ro_cursor(db)
            .map_err(|e| commit_error("open verify cursor", e))?;
        match cursor.get(None, None, MDB_LAST) {
            Ok((Some(key), _)) => Some(key.to_vec()),
            Ok((None, _)) | Err(lmdb::Error::NotFound) => None,
            Err(e) => return Err(commit_error("read last key", e)),
        }
    };
    let ok = match (last.as_deref(), loaded_through) {
        // Empty tree: a fresh merge, or a resumed one that appended nothing yet.
        (None, _) => true,
        // Resumed merge: the tree must hold only keys up to the resume mark.
        (Some(db_last), Some(mark)) => db_last <= mark,
        // A populated tree with no resume mark would be appended over — refuse.
        (Some(_), None) => false,
    };
    if ok {
        Ok(())
    } else {
        Err(CommitError::WriteFailed {
            operation: "verify target namespace empty before merge",
            source: format!("namespace {ns} is not empty at merge start").into(),
        })
    }
}

/// Delete a manifest key, treating an absent key as already done.
fn delete_if_present(
    txn: &mut RwTransaction<'_>,
    meta_db: Database,
    key: &[u8],
) -> Result<(), CommitError> {
    match txn.del(meta_db, &key, None) {
        Ok(()) | Err(lmdb::Error::NotFound) => Ok(()),
        Err(e) => Err(commit_error("delete manifest entry", e)),
    }
}

/// A [`CommitError::DeferredLogCorrupt`] from a segment decode failure.
fn corrupt(ns: Namespace, source: log::SegmentError) -> CommitError {
    CommitError::DeferredLogCorrupt {
        namespace: ns.to_string(),
        source: Box::new(source),
    }
}

/// A [`CommitError::DeferredLogCorrupt`] from an I/O failure reading the log.
fn corrupt_io(ns: Namespace, source: std::io::Error) -> CommitError {
    CommitError::DeferredLogCorrupt {
        namespace: ns.to_string(),
        source: Box::new(source),
    }
}

/// A [`CommitError::DeferredLogCorrupt`] from a structural invariant breach.
fn corrupt_msg(ns: Namespace, message: &'static str) -> CommitError {
    CommitError::DeferredLogCorrupt {
        namespace: ns.to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::deferred::fault;
    use crate::deferred::test_support::{open, put, SCAT};
    use zaino_persistence::{Backend, BackendReader, BackendWriter, BulkPolicy, RawKey, RawValue};

    /// Scan the scattered namespace to a sorted vec of owned pairs.
    fn scan_scat(backend: &crate::LmdbBackend) -> Vec<(RawKey, RawValue)> {
        backend.reader().expect("reader").scan(SCAT).expect("scan")
    }

    #[test]
    fn equal_key_across_segments_takes_the_later_value() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = open(tmp.path());
        backend
            .begin_bulk(BulkPolicy { enabled: true })
            .expect("begin_bulk");

        let mut writer = backend.writer().expect("writer");
        // Two commits write the same key: one segment each, later segment wins.
        writer
            .commit(vec![put(SCAT, vec![0x05], b"first")])
            .expect("commit one");
        writer
            .commit(vec![put(SCAT, vec![0x05], b"second")])
            .expect("commit two");
        drop(writer);

        backend.finish_bulk().expect("finish_bulk");
        assert_eq!(
            scan_scat(&backend),
            vec![(vec![0x05], b"second".to_vec())],
            "the later segment's value wins for an equal key"
        );
    }

    #[test]
    fn crash_mid_finish_resumes_without_skip_or_duplicate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let expected: Vec<(RawKey, RawValue)> = vec![
            (vec![0x01], b"v1".to_vec()),
            (vec![0x02], b"v2".to_vec()),
            (vec![0x03], b"v3".to_vec()),
            (vec![0x04], b"v4".to_vec()),
            (vec![0x05], b"v5".to_vec()),
        ];

        // Build two segments of scattered keys, then interrupt the merge.
        {
            let backend = open(tmp.path());
            backend
                .begin_bulk(BulkPolicy { enabled: true })
                .expect("begin_bulk");
            let mut writer = backend.writer().expect("writer");
            writer
                .commit(vec![
                    put(SCAT, vec![0x01], b"v1"),
                    put(SCAT, vec![0x03], b"v3"),
                ])
                .expect("commit one");
            writer
                .commit(vec![
                    put(SCAT, vec![0x02], b"v2"),
                    put(SCAT, vec![0x04], b"v4"),
                    put(SCAT, vec![0x05], b"v5"),
                ])
                .expect("commit two");
            drop(writer);
            backend.flush().expect("flush");

            // A tiny chunk forces several chunk transactions; stop after the first.
            fault::set_chunk_size(2);
            fault::arm_stop_after_chunk(1);
            backend.finish_bulk().expect("interrupted finish returns");
            assert!(
                !backend
                    .reader()
                    .expect("reader")
                    .is_complete(SCAT)
                    .expect("is_complete"),
                "the namespace is still incomplete after an interrupted finish"
            );
        }

        // Reopen and finish: the merge resumes past the recorded mark.
        let backend = open(tmp.path());
        fault::set_chunk_size(2);
        backend.finish_bulk().expect("resumed finish_bulk");
        assert!(
            backend
                .reader()
                .expect("reader")
                .is_complete(SCAT)
                .expect("is_complete"),
            "the namespace is complete after the resumed finish"
        );
        assert_eq!(
            scan_scat(&backend),
            expected,
            "the resumed build equals a direct build: no key skipped or duplicated"
        );
    }
}
