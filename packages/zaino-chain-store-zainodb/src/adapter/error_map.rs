//! This backend's failures, as the domain names them.
//!
//! One direction only: a `StoreError` becomes a `ChainStoreError` or a
//! `ChainStoreSourceError`, and nothing here goes the other way. The mapping is
//! deliberately narrow — only the variants carrying a domain meaning are
//! translated — and the rest arrive opaque but not silent, carrying their cause
//! for the operator's log.

use zaino_chain_store::{ChainStoreError, ChainStoreSourceError, StoreCapability};

use crate::error::StoreError;
use crate::store::capability::CapabilityRequest;

/// This backend's error, as the domain names it.
///
/// The mapping is deliberately narrow. Only the variants that carry a domain
/// meaning are translated; everything else becomes
/// [`ChainStoreError::Backend`], whose contract is that it is opaque and must
/// not be branched on. Inventing a domain meaning for an LMDB error would give
/// a consumer something to branch on that this backend cannot promise another
/// one would produce.
///
/// Narrow is not the same as lossy, though. The untranslated error is handed
/// over whole as the cause, so an operator reading the log still reaches the
/// LMDB errno underneath it. Rendering it with `to_string` instead would keep
/// only the top line and drop that error's own `source`, which is where the
/// actual failure usually is.
pub(super) fn chain_store_error(error: StoreError) -> ChainStoreError {
    match error {
        StoreError::DataUnavailable(what) => ChainStoreError::MissingRow(what),
        StoreError::FeatureUnavailable(request) => {
            ChainStoreError::Unavailable(capability_for_feature(request))
        }
        StoreError::V1BackendUnavailable(_) => ChainStoreError::NotReady,
        other => ChainStoreError::backend_because(other.to_string(), other),
    }
}

/// Which domain capability a refusal was about.
///
/// The refusal carries the [`CapabilityRequest`] itself, so producer and matcher
/// cannot drift: the crate's finer request vocabulary is folded onto the
/// domain's coarser one here, and the match is total. The three index requests
/// name their own domain capability; every core and block-extension request
/// folds to [`StoreCapability::Core`], the one capability every store must
/// have, so a caller routes elsewhere rather than retrying.
///
/// The grouping is explicit rather than a `_` catch-all so that adding a
/// [`CapabilityRequest`] variant is a compile error here until it is classified,
/// rather than silently collapsing to `Core`.
fn capability_for_feature(request: CapabilityRequest) -> StoreCapability {
    match request {
        CapabilityRequest::SpentOutputIndex => StoreCapability::SpentOutputs,
        CapabilityRequest::TxOutSetIndex => StoreCapability::TxOutSet,
        CapabilityRequest::TransparentHistIndex => StoreCapability::TransparentHistory,
        CapabilityRequest::ReadCore
        | CapabilityRequest::WriteCore
        | CapabilityRequest::BlockCoreExt
        | CapabilityRequest::BlockTransparentExt
        | CapabilityRequest::BlockShieldedExt
        | CapabilityRequest::CompactBlockExt
        | CapabilityRequest::IndexedBlockExt => StoreCapability::Core,
    }
}

/// A corrupt row, reported to the operator on its way to the caller.
///
/// # Why the logging is here rather than at each site
///
/// A corrupt row is the one read failure nothing upstream can act on. The
/// caller's recovery is to fall through to the validator, which is correct and
/// silent — so without a log here a store that is quietly rotting is
/// indistinguishable from one that is merely behind, for as long as the
/// validator keeps covering. The read path has no other place this surfaces:
/// the error is converted to a domain error, then to a status, and by then the
/// cause naming the field is gone.
///
/// Centralised so every conversion in this file reports identically and no new
/// one can be added that forgets to. `warn` rather than `error`: the request is
/// still answered, from elsewhere.
pub(super) fn corrupt_row(expected: impl Into<String>) -> ChainStoreError {
    let error = ChainStoreError::corrupt_row(expected);
    report_corrupt_row(&error);
    error
}

/// A corrupt row whose rejecting conversion had a typed error to explain it.
pub(super) fn corrupt_row_because(
    expected: impl Into<String>,
    cause: impl std::error::Error + Send + Sync + 'static,
) -> ChainStoreError {
    let error = ChainStoreError::corrupt_row_because(expected, cause);
    report_corrupt_row(&error);
    error
}

/// Logs and counts a corrupt row.
fn report_corrupt_row(error: &ChainStoreError) {
    tracing::warn!(
        error = error as &dyn std::error::Error,
        "chain store read a row it cannot decode"
    );
    #[cfg(feature = "prometheus")]
    metrics::counter!(crate::metric_names::DB_CORRUPT_ROWS_TOTAL).increment(1);
}

/// A source failure, as the domain names it.
///
/// A validator failure is already the domain's, so it passes through untouched.
/// Anything else failed locally while committing, and is carried as the cause
/// for the same reason as in [`chain_store_error`].
pub(super) fn chain_store_source_error(error: StoreError) -> ChainStoreSourceError {
    match error {
        StoreError::Source(source) => source,
        other => ChainStoreSourceError::commit_because(other.to_string(), other),
    }
}

#[cfg(test)]
mod tests {
    use super::super::to_domain::domain_height;
    use super::*;
    use crate::types::Height;

    /// A corrupt row is reported to the operator, not only to the caller.
    ///
    /// The caller's recovery is to fall through to the validator, which is
    /// silent by design — so this log is the only thing that distinguishes a
    /// store that is rotting from one that is merely behind. Asserted through
    /// a subscriber rather than by reading the code, because the reporting is
    /// a side effect and nothing else would notice it being dropped.
    #[test]
    fn a_corrupt_row_is_reported_to_the_operator() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("capture buffer mutex poisoned")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for Captured {
            type Writer = Self;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let _ = domain_height(Height(u32::MAX));
        });

        let logged = String::from_utf8(
            captured
                .0
                .lock()
                .expect("capture buffer mutex poisoned")
                .clone(),
        )
        .expect("log output is utf-8");

        assert!(logged.contains("WARN"), "not logged at warn: {logged:?}");
        assert!(
            logged.contains("cannot decode"),
            "the corrupt row was not reported: {logged:?}"
        );
    }

    /// An untranslated backend failure reaches the domain with its cause.
    ///
    /// The boundary is meant to be opaque to *branching*, not to reading. An
    /// earlier version rendered the error with `to_string`, which kept the top
    /// line and dropped the error's own `source` — so an operator logging the
    /// chain got the summary and none of the LMDB detail underneath it.
    #[test]
    fn an_untranslated_failure_carries_its_cause() {
        use std::error::Error as _;

        let error = chain_store_error(StoreError::LmdbError(lmdb::Error::Panic));

        let ChainStoreError::Backend { ref message, .. } = error else {
            panic!("an LMDB failure has no domain meaning, so it must be Backend");
        };
        assert!(message.contains("LMDB"), "message was {message:?}");

        let cause = error.source().expect("the backend error must be carried");
        assert!(
            cause.to_string().contains("MDB_PANIC"),
            "cause was {cause:?}, which does not name the LMDB failure"
        );
    }

    /// A commit failure carries its cause the same way.
    #[test]
    fn a_failed_commit_carries_its_cause() {
        use std::error::Error as _;

        let error = chain_store_source_error(StoreError::LmdbError(lmdb::Error::Panic));

        assert!(matches!(error, ChainStoreSourceError::Commit { .. }));
        assert!(error
            .source()
            .expect("the backend error must be carried")
            .to_string()
            .contains("MDB_PANIC"));
    }

    /// A validator failure is already the domain's, so it is not re-wrapped.
    #[test]
    fn a_validator_failure_passes_through() {
        let error = chain_store_source_error(StoreError::Source(
            ChainStoreSourceError::unavailable("no route to validator"),
        ));

        assert!(matches!(error, ChainStoreSourceError::Unavailable { .. }));
    }

    /// Each index request names the capability the caller was denied.
    ///
    /// The refusal carries the [`CapabilityRequest`] itself, so producer and
    /// matcher share one vocabulary and cannot drift — the failure an earlier,
    /// string-matching version had, where literals the producer never emitted
    /// read as correct while every real refusal collapsed to `Core`.
    #[test]
    fn an_index_refusal_maps_to_the_capability_it_denied() {
        assert_eq!(
            capability_for_feature(CapabilityRequest::SpentOutputIndex),
            StoreCapability::SpentOutputs
        );
        assert_eq!(
            capability_for_feature(CapabilityRequest::TxOutSetIndex),
            StoreCapability::TxOutSet
        );
        assert_eq!(
            capability_for_feature(CapabilityRequest::TransparentHistIndex),
            StoreCapability::TransparentHistory
        );
    }

    /// Every core and block-extension request folds to `Core`.
    ///
    /// The domain does not distinguish these, so a refusal of any of them is
    /// reported as the one capability every store must have; the caller routes
    /// elsewhere rather than over-claiming which index is missing. The typed,
    /// total match replaces the old catch-all — there is no longer an
    /// "unrecognised name" case, because there is no name.
    #[test]
    fn a_core_or_block_refusal_maps_to_core() {
        for request in [
            CapabilityRequest::ReadCore,
            CapabilityRequest::WriteCore,
            CapabilityRequest::BlockCoreExt,
            CapabilityRequest::BlockTransparentExt,
            CapabilityRequest::BlockShieldedExt,
            CapabilityRequest::CompactBlockExt,
            CapabilityRequest::IndexedBlockExt,
        ] {
            assert_eq!(capability_for_feature(request), StoreCapability::Core);
        }
    }

    /// A missing v1 backend is transient, so it reaches the domain as `NotReady`.
    ///
    /// It is a backend-state refusal, not a capability one: the concrete v1
    /// backend is absent only while a migration runs and returns once it
    /// completes, which is exactly what [`ChainStoreError::NotReady`] means.
    #[test]
    fn a_missing_v1_backend_maps_to_not_ready() {
        let error = chain_store_error(StoreError::V1BackendUnavailable(
            "v1 spent db not available",
        ));
        assert!(matches!(error, ChainStoreError::NotReady));
    }
}
