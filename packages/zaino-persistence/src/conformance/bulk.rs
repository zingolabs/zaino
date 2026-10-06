//! The bulk-mode properties of the [`Backend`] contract.
//!
//! These hold for a backend that ignores bulk mode (the in-memory backend) and
//! for one that defers (LMDB): they are written against the contract, not an
//! implementation. In particular [`is_complete`](crate::BackendReader::is_complete)
//! for a `Scattered` namespace *inside* bulk mode is left to the backend — the
//! suite accepts either answer — while walk-ordered and meta namespaces must
//! stay complete throughout, and every namespace must be complete once
//! `finish_bulk` returns.

use super::support;
use super::BackendFactory;
use crate::backend::{Backend, BackendReader, BackendWriter, BulkPolicy, KeyOrder};

/// With deferral disabled, the backend behaves exactly like a direct build: every
/// commit is visible without `finish_bulk`, and every namespace stays complete.
pub fn bulk_disabled_matches_direct<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let batches = support::bulk_batches();
    let expected = support::oracle(&batches);

    let backend = factory.fresh(&specs);
    backend
        .begin_bulk(BulkPolicy { enabled: false })
        .expect("begin_bulk (disabled)");
    commit_all(&backend, batches);

    support::assert_matches_oracle(&backend, &specs, &expected);
    let reader = backend.reader().expect("reader");
    for spec in &specs {
        assert!(
            reader.is_complete(spec.namespace).expect("is_complete"),
            "disabled bulk mode leaves {} complete",
            spec.namespace
        );
    }
}

/// With deferral enabled, after `finish_bulk` every namespace scans byte-for-byte
/// equal to a direct build of the same commits.
pub fn bulk_enabled_after_finish_matches_direct<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let batches = support::bulk_batches();
    let expected = support::oracle(&batches);

    let backend = factory.fresh(&specs);
    backend
        .begin_bulk(BulkPolicy { enabled: true })
        .expect("begin_bulk");
    commit_all(&backend, batches);
    backend.finish_bulk().expect("finish_bulk");

    support::assert_matches_oracle(&backend, &specs, &expected);
}

/// Outside bulk mode, every namespace is complete — even right after a plain
/// commit, with no `begin_bulk` ever called.
pub fn is_complete_true_outside_bulk_mode<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    commit_all(&backend, support::bulk_batches());

    let reader = backend.reader().expect("reader");
    for spec in &specs {
        assert!(
            reader.is_complete(spec.namespace).expect("is_complete"),
            "outside bulk mode, {} is complete",
            spec.namespace
        );
    }
}

/// Inside bulk mode: walk-ordered and meta namespaces stay complete, a scattered
/// namespace may read either way (the backend's choice to defer or not), and
/// after `finish_bulk` every namespace is complete again.
pub fn is_complete_inside_bulk_mode<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let backend = factory.fresh(&specs);
    backend
        .begin_bulk(BulkPolicy { enabled: true })
        .expect("begin_bulk");
    commit_all(&backend, support::bulk_batches());

    {
        let reader = backend.reader().expect("reader");
        for spec in &specs {
            let complete = reader.is_complete(spec.namespace).expect("is_complete");
            match spec.key_order {
                KeyOrder::WalkOrdered | KeyOrder::Meta => assert!(
                    complete,
                    "{} ({:?}) must stay complete in bulk mode",
                    spec.namespace, spec.key_order
                ),
                // A deferring backend reports false here; a non-deferring one
                // reports true. Both are contract-legal, so assert neither.
                KeyOrder::Scattered => {}
            }
        }
    }

    backend.finish_bulk().expect("finish_bulk");
    let reader = backend.reader().expect("reader");
    for spec in &specs {
        assert!(
            reader.is_complete(spec.namespace).expect("is_complete"),
            "after finish_bulk, {} must be complete",
            spec.namespace
        );
    }
}

/// `finish_bulk` is idempotent: a second call succeeds, leaves the data equal to
/// a direct build, and every namespace complete.
pub fn finish_bulk_is_idempotent<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let batches = support::bulk_batches();
    let expected = support::oracle(&batches);

    let backend = factory.fresh(&specs);
    backend
        .begin_bulk(BulkPolicy { enabled: true })
        .expect("begin_bulk");
    commit_all(&backend, batches);
    backend.finish_bulk().expect("first finish_bulk");
    backend
        .finish_bulk()
        .expect("second finish_bulk is a no-op, not an error");

    support::assert_matches_oracle(&backend, &specs, &expected);
    let reader = backend.reader().expect("reader");
    for spec in &specs {
        assert!(
            reader.is_complete(spec.namespace).expect("is_complete"),
            "{} complete after an idempotent finish",
            spec.namespace
        );
    }
}

/// A restart in the middle of bulk mode resumes on the existing deferral state:
/// begin, commit, reopen, begin again, commit the rest, finish — equal to a
/// direct build of all the commits. Skipped (a no-op) when `reopen` is `None`.
pub fn restart_in_bulk_mode_matches_direct<F: BackendFactory>(factory: &F) {
    let specs = support::specs();
    let batches = support::bulk_batches();
    let expected = support::oracle(&batches);

    let mut batches = batches.into_iter();
    let first = batches.next().expect("bulk_batches has at least one batch");
    let rest: Vec<_> = batches.collect();

    {
        let backend = factory.fresh(&specs);
        backend
            .begin_bulk(BulkPolicy { enabled: true })
            .expect("begin_bulk");
        let mut writer = backend.writer().expect("writer");
        writer.commit(first).expect("commit before restart");
        drop(writer);
        backend.flush().expect("flush");
    }

    let Some(backend) = factory.reopen(&specs) else {
        return;
    };
    // Re-enter bulk mode on the persisted deferral state rather than starting a
    // fresh one.
    backend
        .begin_bulk(BulkPolicy { enabled: true })
        .expect("re-enter bulk mode after restart");
    commit_all(&backend, rest);
    backend.finish_bulk().expect("finish_bulk");

    support::assert_matches_oracle(&backend, &specs, &expected);
}

/// Commit every batch in order through one writer.
fn commit_all<B: Backend>(backend: &B, batches: Vec<Vec<crate::backend::WriteOp>>) {
    let mut writer = backend.writer().expect("writer");
    for batch in batches {
        writer.commit(batch).expect("commit");
    }
}
