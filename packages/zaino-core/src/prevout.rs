//! Resolving transparent inputs to the outputs they spend.
//!
//! A transparent input names the output it spends only by outpoint; the explorer
//! wire shape needs that output's value and script. Resolution looks first in the
//! same set of transactions — a block spends its own earlier outputs — and
//! otherwise fetches the spent transaction. The fetch is abstracted behind a
//! closure so this logic is pure: the engine supplies a closure over the
//! passthrough provider, and tests supply a counting one.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::future::Future;

use futures::stream::{self, StreamExt, TryStreamExt};

use zaino_primitives::types::{Transaction, TransactionId, TransparentOutput};
use zaino_service::ResolvedInput;
use zaino_service::error::TransactionViewError;

/// How many distinct prevout transactions are fetched at once.
///
/// While prevouts are resolved by passthrough — one validator round trip per
/// distinct external txid — this bounds the fan-out of a single `getblock(_, 2)`
/// so one request cannot flood the validator. A local outpoint index removes the
/// fetch entirely, at which point this bound no longer applies.
pub(crate) const PREVOUT_FETCH_CONCURRENCY: usize = 16;

/// Resolve every transparent input of `transactions` to the output it spends,
/// returning one resolved-input list per transaction, in the same order.
///
/// A prevout found among `transactions` themselves (an intra-block spend) is
/// resolved with no fetch. The remaining distinct prevout txids are fetched
/// through `fetch` — deduplicated, and at most [`PREVOUT_FETCH_CONCURRENCY`] at
/// a time. `fetch` yields the spent transaction's outputs, `Ok(None)` when the
/// validator does not know that txid, and `Err` for a transport failure.
///
/// A prevout the validator cannot find is a [`TransactionViewError::MissingPrevout`];
/// one whose index is past the spent transaction's outputs is a
/// [`TransactionViewError::PrevoutIndexOutOfRange`]; a transport failure is a
/// [`TransactionViewError::Unavailable`] keeping the cause as its source chain.
pub(crate) async fn resolve_prevouts<Fetch, Fut>(
    transactions: &[Transaction],
    fetch: Fetch,
) -> Result<Vec<Vec<ResolvedInput>>, TransactionViewError>
where
    Fetch: Fn(TransactionId) -> Fut,
    Fut: Future<Output = Result<Option<Vec<TransparentOutput>>, Box<dyn Error + Send + Sync>>>,
{
    // The spends this set resolves without a fetch: each transaction's own
    // outputs, keyed by txid.
    let intra: HashMap<TransactionId, &[TransparentOutput]> = transactions
        .iter()
        .map(|tx| (tx.txid, tx.transparent.outputs.as_slice()))
        .collect();

    // The distinct prevout txids this set does not already hold, in first-seen
    // order — one fetch each, never repeated across inputs that share a prevout.
    let external: Vec<TransactionId> = {
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        for tx in transactions {
            for input in &tx.transparent.inputs {
                if !intra.contains_key(&input.prev_txid) && seen.insert(input.prev_txid) {
                    ordered.push(input.prev_txid);
                }
            }
        }
        ordered
    };

    let fetch = &fetch;
    let fetched: HashMap<TransactionId, Option<Vec<TransparentOutput>>> = stream::iter(external)
        .map(move |txid| async move { fetch(txid).await.map(|outputs| (txid, outputs)) })
        .buffer_unordered(PREVOUT_FETCH_CONCURRENCY)
        .try_collect()
        .await
        .map_err(|cause| TransactionViewError::Unavailable { cause })?;

    transactions
        .iter()
        .map(|tx| {
            tx.transparent
                .inputs
                .iter()
                .map(|input| {
                    let outputs: &[TransparentOutput] = match intra.get(&input.prev_txid) {
                        Some(outputs) => outputs,
                        None => match fetched.get(&input.prev_txid) {
                            Some(Some(outputs)) => outputs.as_slice(),
                            Some(None) | None => {
                                return Err(TransactionViewError::MissingPrevout {
                                    outpoint: input.clone(),
                                });
                            }
                        },
                    };
                    match usize::try_from(input.prev_index)
                        .ok()
                        .and_then(|index| outputs.get(index))
                    {
                        Some(output) => Ok(ResolvedInput {
                            outpoint: input.clone(),
                            spent: output.clone(),
                        }),
                        None => Err(TransactionViewError::PrevoutIndexOutOfRange {
                            outpoint: input.clone(),
                            index: input.prev_index,
                            outputs: outputs.len(),
                        }),
                    }
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()
}

#[cfg(test)]
mod tests {
    use super::{PREVOUT_FETCH_CONCURRENCY, resolve_prevouts};
    use std::error::Error;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use zaino_primitives::types::{
        Script, Transaction, TransactionId, TransparentData, TransparentInput, TransparentOutput,
        Zatoshis,
    };
    use zaino_service::error::TransactionViewError;

    /// What a fetcher yields: the spent transaction's outputs, `None` on a miss,
    /// or a boxed transport cause.
    type FetchResult = Result<Option<Vec<TransparentOutput>>, Box<dyn Error + Send + Sync>>;

    fn txid(byte: u8) -> TransactionId {
        TransactionId::from([byte; 32])
    }

    /// An output whose value doubles as a tag, so a resolved input can be matched
    /// back to the exact output it should have spent.
    fn output(tag: u64) -> TransparentOutput {
        TransparentOutput {
            value: Zatoshis::new(tag).expect("tag in range"),
            script: Script::new(vec![0x76, 0xa9]),
        }
    }

    fn input(prev: u8, index: u32) -> TransparentInput {
        TransparentInput {
            prev_txid: txid(prev),
            prev_index: index,
        }
    }

    fn tx(id: u8, inputs: Vec<TransparentInput>, outputs: Vec<TransparentOutput>) -> Transaction {
        Transaction {
            txid: txid(id),
            transparent: TransparentData { inputs, outputs },
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    /// A transport failure carrying a recognisable concrete cause, so a test can
    /// walk `source()` and prove the chain survived the wrappers.
    #[derive(Debug)]
    struct FetchCause;

    impl std::fmt::Display for FetchCause {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "counting fetcher transport failure")
        }
    }

    impl Error for FetchCause {}

    /// A fetcher that counts its calls and answers from a fixed txid→outputs map;
    /// a txid absent from the map is a miss (`Ok(None)`).
    struct Counting {
        calls: AtomicUsize,
        known: Vec<(TransactionId, Vec<TransparentOutput>)>,
    }

    impl Counting {
        fn new(known: Vec<(TransactionId, Vec<TransparentOutput>)>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                known,
            }
        }

        fn fetcher(&self) -> impl Fn(TransactionId) -> std::future::Ready<FetchResult> + '_ {
            move |id| {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let outputs = self
                    .known
                    .iter()
                    .find(|(known, _)| *known == id)
                    .map(|(_, outputs)| outputs.clone());
                std::future::ready(Ok(outputs))
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    /// A fetcher that always fails the transport, for the `Unavailable` path.
    fn failing_fetch(_id: TransactionId) -> std::future::Ready<FetchResult> {
        std::future::ready(Err(Box::new(FetchCause)))
    }

    #[tokio::test]
    async fn a_same_block_prevout_resolves_with_no_fetch() {
        // Transaction `0xAA` has an output; `0xBB`, later in the same block,
        // spends it. The spend resolves from the block, with zero fetches.
        let source = tx(0xAA, Vec::new(), vec![output(100)]);
        let spender = tx(0xBB, vec![input(0xAA, 0)], Vec::new());
        let counting = Counting::new(Vec::new());

        let resolved = resolve_prevouts(&[source, spender], counting.fetcher())
            .await
            .expect("resolved");

        assert_eq!(counting.calls(), 0, "an intra-block prevout is not fetched");
        assert_eq!(resolved[0], Vec::new());
        assert_eq!(resolved[1].len(), 1);
        assert_eq!(resolved[1][0].spent, output(100));
    }

    #[tokio::test]
    async fn two_inputs_on_one_external_txid_fetch_once() {
        // Both inputs spend outputs of the same external transaction `0xCC`; it is
        // fetched exactly once, and each input resolves to its own output.
        let spender = tx(0x01, vec![input(0xCC, 0), input(0xCC, 1)], Vec::new());
        let counting = Counting::new(vec![(txid(0xCC), vec![output(10), output(20)])]);

        let resolved = resolve_prevouts(&[spender], counting.fetcher())
            .await
            .expect("resolved");

        assert_eq!(counting.calls(), 1, "a shared prevout txid is fetched once");
        assert_eq!(resolved[0][0].spent, output(10));
        assert_eq!(resolved[0][1].spent, output(20));
    }

    #[tokio::test]
    async fn a_missing_prevout_names_the_outpoint() {
        // The validator does not know the spent txid: a source inconsistency named
        // by its outpoint, not a blank value.
        let spender = tx(0x01, vec![input(0xDD, 3)], Vec::new());
        let counting = Counting::new(Vec::new());

        match resolve_prevouts(&[spender], counting.fetcher()).await {
            Err(TransactionViewError::MissingPrevout { outpoint }) => {
                assert_eq!(outpoint, input(0xDD, 3));
            }
            other => panic!("expected MissingPrevout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_out_of_range_index_is_its_own_variant() {
        // The spent transaction exists but has one output; index 5 is past it.
        let spender = tx(0x01, vec![input(0xEE, 5)], Vec::new());
        let counting = Counting::new(vec![(txid(0xEE), vec![output(7)])]);

        match resolve_prevouts(&[spender], counting.fetcher()).await {
            Err(TransactionViewError::PrevoutIndexOutOfRange {
                outpoint,
                index,
                outputs,
            }) => {
                assert_eq!(outpoint, input(0xEE, 5));
                assert_eq!(index, 5);
                assert_eq!(outputs, 1);
            }
            other => panic!("expected PrevoutIndexOutOfRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_transport_failure_is_unavailable_with_the_cause_kept() {
        let spender = tx(0x01, vec![input(0xFF, 0)], Vec::new());

        match resolve_prevouts(&[spender], failing_fetch).await {
            Err(TransactionViewError::Unavailable { cause }) => {
                // Walk the source chain and prove the concrete cause survived.
                let mut cursor: Option<&(dyn Error + 'static)> = Some(cause.as_ref());
                let mut reached = false;
                while let Some(err) = cursor {
                    if err.downcast_ref::<FetchCause>().is_some() {
                        reached = true;
                        break;
                    }
                    cursor = err.source();
                }
                assert!(reached, "the concrete transport cause must stay reachable");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_coinbase_has_no_inputs_to_resolve_and_fetches_nothing() {
        // A coinbase carries its input in the detail, not here, so the indexing
        // shape has no transparent inputs — nothing to resolve, nothing to fetch.
        let coinbase = tx(0xAA, Vec::new(), vec![output(5000)]);
        let counting = Counting::new(Vec::new());

        let resolved = resolve_prevouts(&[coinbase], counting.fetcher())
            .await
            .expect("resolved");

        assert_eq!(counting.calls(), 0);
        assert_eq!(resolved, vec![Vec::new()]);
    }

    #[tokio::test]
    async fn resolved_inputs_keep_input_order() {
        // Inputs reference external txids out of fetch-completion order; the
        // resolved list still follows the input order, not fetch order.
        let spender = tx(
            0x01,
            vec![input(0xA1, 0), input(0xA2, 0), input(0xA1, 1)],
            Vec::new(),
        );
        let counting = Counting::new(vec![
            (txid(0xA1), vec![output(11), output(12)]),
            (txid(0xA2), vec![output(21)]),
        ]);

        let resolved = resolve_prevouts(&[spender], counting.fetcher())
            .await
            .expect("resolved");

        let spent: Vec<_> = resolved[0].iter().map(|r| r.spent.clone()).collect();
        assert_eq!(spent, vec![output(11), output(21), output(12)]);
    }

    #[test]
    fn the_fetch_concurrency_bound_is_the_documented_value() {
        assert_eq!(PREVOUT_FETCH_CONCURRENCY, 16);
    }
}
