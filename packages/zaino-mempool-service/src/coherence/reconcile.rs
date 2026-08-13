//! Reconcile (freeze / thaw): recomputing the coherent view from the core
//! snapshot and the observed tips, and classifying why a view must freeze.

use zaino_mempool::ports::{Mempool, NfsEpochObserver, NonFinalizedEpoch};
use zaino_mempool::snapshot::MempoolSnapshot;
use zaino_mempool::tip::{FreezeReason, ObservedTips, TipChange};

impl<M: Mempool, N: NfsEpochObserver> super::CoherenceService<M, N> {
    /// Recompute the coherent view from the core's current snapshot and the NS
    /// epoch. Idempotent: publishes (and emits an event) only on a state change.
    pub(super) fn reconcile(&self) {
        let core = self.mempool.current();
        let observed = self.observe_tips(&core);
        let prev = self.coherent.load_full();

        // Nothing has been polled yet, so there is no set to bless. The empty
        // pre-first-poll snapshot must never be served as a coherent view: it
        // would tell a caller their transaction is not pending when Zaino has
        // simply not looked.
        //
        // The tip path below would also catch this — an unready snapshot has no
        // `source_tip`, so `observe_tips` yields no validator tip and `agree()`
        // returns `None`. Stated here anyway: the safety of the empty case
        // should not depend on following that through three other functions, and
        // a future change to `observe_tips` must not be able to quietly remove
        // it. Costs one branch on a path that runs once per publication.
        if !core.is_ready() {
            self.freeze(&prev, observed, FreezeReason::ValidatorTipUnavailable);
            return;
        }

        // Freeze only when the set may be *wrong*, not merely *short*.
        //
        // Freezing does not make missing transactions appear — it withholds the
        // ones the core does have on top of the ones it doesn't. A set that is
        // short by a known, named list (capacity-refused or metadata-deferred;
        // see `MempoolSnapshot::unadmitted`) is still an accurate view of what it
        // holds and is tagged with a sound tip, so serving it is strictly more
        // useful than a blackout. A set that may not reflect the source at all is
        // the case a freeze is actually for.
        if core.completeness().may_be_wrong() {
            self.freeze(&prev, observed, FreezeReason::CoreIncomplete);
            return;
        }

        match observed.agree() {
            Some(epoch) => self.publish_live(&prev, core, observed, epoch),
            None => {
                let reason = Self::freeze_reason_from_tips(prev.observed_tips, observed);
                self.freeze(&prev, observed, reason);
            }
        }
    }

    fn observe_tips(&self, core: &MempoolSnapshot) -> ObservedTips {
        let validator = core.source_tip();

        let non_finalized = match &self.nfs {
            // Dual-tip: the observer reports the ChainIndex epoch (`None` freezes).
            Some(observer) => observer.current_epoch(),
            // Validator-only: synthesize the epoch from the validator tip.
            None => validator.map(|tip| self.synthesized_epoch(tip)),
        };

        ObservedTips {
            validator,
            non_finalized,
        }
    }

    fn synthesized_epoch(
        &self,
        validator_tip: zaino_primitives::types::BlockRef,
    ) -> NonFinalizedEpoch {
        let mut state = self.synth_epoch.lock().expect("synth epoch lock poisoned");
        if state.last_validator_hash != Some(validator_tip.hash) {
            state.generation = state.generation.saturating_add(1);
            state.last_validator_hash = Some(validator_tip.hash);
        }
        NonFinalizedEpoch {
            generation: state.generation,
            best_tip: validator_tip,
        }
    }

    fn classify_tip_change(previous: ObservedTips, next: ObservedTips) -> TipChange {
        let validator_changed = previous.validator != next.validator;
        let ns_changed = previous.non_finalized != next.non_finalized;
        match (validator_changed, ns_changed) {
            (false, false) => TipChange::None,
            (true, false) => TipChange::ValidatorChanged,
            (false, true) => TipChange::NonFinalizedChanged,
            (true, true) => TipChange::BothChanged,
        }
    }

    fn freeze_reason_from_tips(old_tips: ObservedTips, new_tips: ObservedTips) -> FreezeReason {
        if new_tips.non_finalized.is_none() {
            return FreezeReason::NonFinalizedUnavailable;
        }
        if new_tips.validator.is_none() {
            return FreezeReason::ValidatorTipUnavailable;
        }
        if new_tips.disagree() {
            return FreezeReason::TipsDiverged;
        }
        match Self::classify_tip_change(old_tips, new_tips) {
            TipChange::ValidatorChanged => FreezeReason::ValidatorTipChanged,
            TipChange::NonFinalizedChanged => FreezeReason::NonFinalizedTipChanged,
            TipChange::BothChanged => FreezeReason::BothTipsChanged,
            TipChange::None => FreezeReason::TipsDiverged,
        }
    }
}
