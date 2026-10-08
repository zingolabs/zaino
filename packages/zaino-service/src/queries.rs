//! Domain-side query composition, shared by every serving adapter.
//!
//! These are the questions more than one adapter asks of the same capabilities
//! — "what range can this snapshot answer", "what do these addresses hold in
//! total" — answered once, here, so two adapters cannot drift into two
//! different answers.
//!
//! Free functions over the read traits rather than provided trait methods: a
//! port stays a narrow declaration of what a capability *is*, and composition
//! over several reads is a separate concern that does not belong on it.
//!
//! Two layers live here. The **primitives** — [`serviceable_range`] and
//! [`total_balance`] — compute and nothing more. The **per-use-case** functions
//! — [`address_balance`] and [`wallet_balance`] — own the one policy decision a
//! primitive deliberately leaves open: what an unserviceable snapshot *means*.
//! That decision differs by consumer, and it lives here, in the one file where
//! the difference is visible, rather than in the adapters where it drifted
//! before. An explorer showing zero for an unsynced chain is accurate, while a
//! wallet concluding zero balance from an indexer that cannot answer could
//! report a user's funds as gone. An adapter only renders the domain answer to
//! its wire shape and maps its error codes; the answer itself is decided here.

use std::collections::HashSet;

use zaino_primitives::types::{
    AddressBalance, AddressDelta, Height, HeightRange, TransactionId, TransparentAddress, Utxo,
    Zatoshis, ZatoshisFlowSum,
};

use crate::error::AddressReadError;
use crate::reads::{AddressRead, ReadBudget};
use crate::ChainSegment;

/// Explorer policy: an unserviceable snapshot has no indexed history, which an
/// explorer reads correctly as a zero balance.
///
/// This is the inverse of [`wallet_balance`]: the two consumers answer the same
/// unserviceable snapshot differently on purpose, and the difference is the
/// reason this policy is named here rather than left to the adapter.
pub async fn address_balance<S>(
    snapshot: &S,
    addrs: &[TransparentAddress],
) -> Result<AddressBalance, AddressReadError>
where
    S: AddressRead + ChainSegment,
{
    let Some(range) = serviceable_range(snapshot) else {
        return Ok(AddressBalance {
            balance: Zatoshis::ZERO,
            received: ZatoshisFlowSum::from_summed(0),
        });
    };
    total_balance(snapshot, addrs, range).await
}

/// Wallet policy, deliberately unlike the explorer's: a wallet that reads
/// "zero" off an indexer which cannot answer would report a user's funds as
/// gone. An unserviceable snapshot is `None`, and the caller surfaces it as a
/// failure rather than a balance.
pub async fn wallet_balance<S>(
    snapshot: &S,
    addrs: &[TransparentAddress],
) -> Result<Option<AddressBalance>, AddressReadError>
where
    S: AddressRead + ChainSegment,
{
    match serviceable_range(snapshot) {
        Some(range) => total_balance(snapshot, addrs, range).await.map(Some),
        None => Ok(None),
    }
}

/// The full height range `snapshot` can answer, or `None` when it can answer
/// nothing.
///
/// This is the range to use when a caller supplies none. It is read from
/// [`ChainSegment::coverage`] rather than built from the pinned tip, because a
/// partially-synced snapshot has a tip it cannot serve all the way down to.
pub fn serviceable_range<S: ChainSegment>(snapshot: &S) -> Option<HeightRange> {
    snapshot.coverage()
}

/// The combined transparent balance of `addrs` over `range`.
///
/// Sums with [`Zatoshis::sum_balances`] and [`ZatoshisFlowSum::checked_join`],
/// so an overflow is reported rather than silently clamped. `balance` is
/// supply-bounded and `received` is a lifetime flow total that is not, which is
/// why the two accumulate through different types.
///
/// An empty `addrs` totals zero: a query about no addresses is well-formed and
/// its answer is zero. A caller for whom an empty list is a protocol error
/// rejects it at its own wire boundary.
///
/// One [`ReadBudget`] spans the whole call, so the combined history read across
/// every address is bounded as a single request rather than per address.
pub async fn total_balance<S: AddressRead>(
    snapshot: &S,
    addrs: &[TransparentAddress],
    range: HeightRange,
) -> Result<AddressBalance, AddressReadError> {
    let mut budget = ReadBudget::for_request();
    let mut balances = Vec::with_capacity(addrs.len());
    let mut received = ZatoshisFlowSum::from_summed(0);
    for addr in addrs {
        let read = snapshot.balance(addr, range, &mut budget).await?;
        balances.push(read.balance);
        received = received.checked_join(read.received).ok_or_else(|| {
            AddressReadError::Fatal("summed lifetime receipts overflow".to_string())
        })?;
    }
    let balance = Zatoshis::sum_balances(balances.into_iter()).ok_or_else(|| {
        AddressReadError::Fatal("summed balance exceeds the money supply".to_string())
    })?;
    Ok(AddressBalance { balance, received })
}

/// The answer to a transparent-address delta query.
///
/// `range` is the range actually queried, and is `None` exactly when no query
/// ran — nothing was serviceable, or the requested bounds were backwards. A
/// caller that echoes the range back to its client has the authoritative value
/// here rather than re-deriving it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressDeltasAnswer {
    /// The deltas, ordered by `(height, block_index, index)`.
    pub deltas: Vec<AddressDelta>,
    /// The range queried, or `None` when no query ran.
    pub range: Option<HeightRange>,
}

/// Every balance change touching `addrs`, over the requested bounds.
///
/// `start` and `end` are inclusive and optional; an absent bound defaults to the
/// snapshot's serviceable edge. Explorer policy, matching
/// [`address_balance`]: nothing serviceable means no indexed history, so the
/// answer is empty rather than an error.
///
/// Backwards bounds answer empty too. Callers derive these from user-supplied
/// dates, where a day with no blocks is an ordinary result and not a fault.
///
/// Ordering is `(height, block_index, index)`, which is what zcashd documents.
/// It is a property of the answer, so it is applied once here rather than in
/// each adapter.
pub async fn address_deltas<S>(
    snapshot: &S,
    addrs: &[TransparentAddress],
    start: Option<Height>,
    end: Option<Height>,
) -> Result<AddressDeltasAnswer, AddressReadError>
where
    S: AddressRead + ChainSegment,
{
    let Some(coverage) = serviceable_range(snapshot) else {
        return Ok(AddressDeltasAnswer {
            deltas: Vec::new(),
            range: None,
        });
    };
    let start = start.unwrap_or(coverage.start);
    let end = end.unwrap_or(coverage.end);
    if start > end {
        return Ok(AddressDeltasAnswer {
            deltas: Vec::new(),
            range: None,
        });
    }
    let range = HeightRange { start, end };

    // One budget for the whole request, so the deltas accumulated across every
    // address are bounded together rather than per address.
    let mut budget = ReadBudget::for_request();
    let mut deltas = Vec::new();
    for addr in addrs {
        deltas.extend(snapshot.deltas(addr, range, &mut budget).await?);
    }
    deltas.sort_by_key(|delta| (delta.height, delta.block_index, delta.index));
    Ok(AddressDeltasAnswer {
        deltas,
        range: Some(range),
    })
}

/// Every transaction id touching `addrs`, over the requested bounds, ordered the
/// way zcashd's `getaddresstxids` orders them.
///
/// `start` and `end` are inclusive and optional, defaulting to the snapshot's
/// serviceable edge — the same range handling as [`address_deltas`], and the
/// same explorer policy: nothing serviceable (or a backwards range) is an empty
/// answer, not an error.
///
/// zcashd builds one ordered set across *all* the requested addresses, keyed on
/// `(height, in-block position, display-hex txid)` and de-duplicated — so a
/// transaction touching two of the addresses appears once, and the union is
/// globally ordered rather than grouped by address. zcashd (like zcashd's own
/// index) orders transactions sharing a height by their **position in the block**,
/// not by txid; the txid is only the final tie-break. This mirrors that key: the
/// per-address `(height, position, txid)` lists are merged, sorted, and
/// de-duplicated by txid; only the bare txids are returned, in that order.
///
/// An entry whose height is unknown (`None`) can only come from a **passthrough**
/// source, which returns bare txids with neither height nor position. Such entries
/// keep the validator's own order and are **not** sorted after the known ones —
/// they sort *before* every `Some`, among themselves in arrival order, with
/// nothing fabricated. Within the known entries, an unknown position (`None`)
/// likewise sorts before a known one at the same height. The node-RPC deployment
/// is `Address = Local`, so in practice every entry carries a height and a
/// position and this is the plain `(height, position, display-hex)` sort.
pub async fn address_txids<S>(
    snapshot: &S,
    addrs: &[TransparentAddress],
    start: Option<Height>,
    end: Option<Height>,
) -> Result<Vec<TransactionId>, AddressReadError>
where
    S: AddressRead + ChainSegment,
{
    let Some(coverage) = serviceable_range(snapshot) else {
        return Ok(Vec::new());
    };
    let start = start.unwrap_or(coverage.start);
    let end = end.unwrap_or(coverage.end);
    if start > end {
        return Ok(Vec::new());
    }
    let range = HeightRange { start, end };

    // One budget for the whole request, so the txids accumulated across every
    // address are bounded together rather than per address.
    let mut budget = ReadBudget::for_request();
    let mut located = Vec::new();
    for addr in addrs {
        located.extend(snapshot.tx_ids(addr, range, &mut budget).await?);
    }
    // A stable sort by a key that is `None` for unknown-height entries and
    // `Some((height, position, display))` for known ones. `Option` orders `None`
    // before `Some`, so unknown-height (passthrough) entries stay ahead of the
    // known ones in their original (validator) order — the stability preserves it,
    // and nothing is invented — while the known ones sort by zcashd's
    // `(height, in-block position, txid)` key. The position (`Option<u32>`) is the
    // primary same-height tie-break, matching zcashd/zcashd's block-position order;
    // `None` positions sort first among a height's entries.
    //
    // zcashd keys the final tie-break on the *display-hex* txid string, not the
    // internal bytes, and lowercase-hex lexical order is the byte order of the
    // reversed (display-order) array. So the key reverses the txid bytes rather
    // than using them as stored.
    located.sort_by_key(|(height, position, txid)| {
        height.map(|h| {
            let mut display = <[u8; 32]>::from(*txid);
            display.reverse();
            (u32::from(h), *position, display)
        })
    });
    let mut seen = HashSet::new();
    Ok(located
        .into_iter()
        .filter_map(|(_, _, txid)| seen.insert(txid).then_some(txid))
        .collect())
}

/// Every unspent transparent output held by `addrs`, ordered the way zcashd's
/// `getaddressutxos` orders them.
///
/// Range-less, because an unspent output is a fact about the current chain, not
/// a window of it — the read itself ([`AddressRead::unspent_outpoints`]) takes no
/// range. Explorer policy, matching [`address_balance`]: an unserviceable
/// snapshot has no indexed history, so the answer is empty rather than an error.
///
/// zcashd merges every requested address's unspent set and sorts it by block
/// height. This mirrors that with a stable sort by height, so a multi-address
/// query returns one height-ordered list rather than one list per address.
pub async fn address_utxos<S>(
    snapshot: &S,
    addrs: &[TransparentAddress],
) -> Result<Vec<Utxo>, AddressReadError>
where
    S: AddressRead + ChainSegment,
{
    if serviceable_range(snapshot).is_none() {
        return Ok(Vec::new());
    }
    // One budget for the whole request, so the unspent sets accumulated across
    // every address are bounded together rather than per address.
    let mut budget = ReadBudget::for_request();
    let mut utxos = Vec::new();
    for addr in addrs {
        utxos.extend(snapshot.unspent_outpoints(addr, &mut budget).await?);
    }
    utxos.sort_by_key(|utxo| u32::from(utxo.height));
    Ok(utxos)
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::{
        address_balance, address_deltas, serviceable_range, total_balance, wallet_balance,
    };
    use crate::testing::{MockChain, MockIndexerService};
    use crate::TakeSnapshot;
    use zaino_primitives::types::{
        AddressBalance, BlockHash, BlockRef, Height, TransparentAddress, Zatoshis, ZatoshisFlowSum,
    };

    /// Two distinct, encodable mainnet transparent addresses. `TransparentAddress`
    /// validates at construction, so a fixture has to be a real address: a
    /// P2PKH and a P2SH, which also keeps the two script types covered.
    const ADDR_A: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
    const ADDR_B: &str = "t3JZcvsuaXE6ygokL4XUiZSTrQBUoPYFnXJ";

    fn t_addr(encoded: &str) -> TransparentAddress {
        TransparentAddress::try_new(encoded).expect("fixture is a valid transparent address")
    }

    fn balance(zats: u64, received: u64) -> AddressBalance {
        AddressBalance {
            balance: Zatoshis::new(zats).expect("valid amount"),
            received: ZatoshisFlowSum::from_summed(received),
        }
    }

    async fn snapshot_with(chain: MockChain) -> impl crate::AddressRead + crate::ChainSegment {
        MockIndexerService::new(chain)
            .snapshot()
            .await
            .expect("snapshot")
    }

    #[tokio::test]
    async fn no_coverage_has_no_serviceable_range() {
        let snapshot = snapshot_with(MockChain::default()).await;
        assert!(serviceable_range(&snapshot).is_none());
    }

    #[tokio::test]
    async fn a_serviceable_range_is_the_snapshots_coverage() {
        let snapshot = snapshot_with(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(10).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            ..Default::default()
        })
        .await;
        let range = serviceable_range(&snapshot).expect("coverage");
        assert_eq!(range.start, Height::GENESIS);
        assert_eq!(u32::from(range.end), 10);
    }

    #[tokio::test]
    async fn total_balance_sums_every_requested_address() {
        let snapshot = snapshot_with(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(10).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            balances: vec![
                (ADDR_A.to_string(), balance(500, 900)),
                (ADDR_B.to_string(), balance(250, 400)),
            ],
            ..Default::default()
        })
        .await;
        let range = serviceable_range(&snapshot).expect("coverage");
        let addrs = vec![t_addr(ADDR_A), t_addr(ADDR_B)];
        let total = total_balance(&snapshot, &addrs, range)
            .await
            .expect("balance");
        assert_eq!(total.balance.as_u64(), 750);
        assert_eq!(u128::from(total.received), 1_300);
    }

    /// An empty address list is a well-formed query with a zero answer. Callers
    /// that want to reject it do so at their own wire boundary, where "you sent
    /// no addresses" is a parameter error.
    #[tokio::test]
    async fn no_addresses_totals_zero() {
        let snapshot = snapshot_with(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(10).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            ..Default::default()
        })
        .await;
        let range = serviceable_range(&snapshot).expect("coverage");
        let total = total_balance(&snapshot, &[], range).await.expect("balance");
        assert_eq!(total.balance.as_u64(), 0);
        assert_eq!(u128::from(total.received), 0);
    }

    /// The two use-case policies answer the same unserviceable snapshot
    /// differently. The snapshot has no coverage (no tip) yet carries a scripted
    /// balance, so a regression that queried a synthesised range instead of
    /// short-circuiting on coverage would read the scripted value and fail here.
    ///
    /// Explorer side: zero is the accurate answer for an unsynced chain.
    #[tokio::test]
    async fn explorer_balance_is_zero_when_nothing_is_serviceable() {
        let snapshot = snapshot_with(MockChain {
            tip: None,
            balances: vec![(ADDR_A.to_string(), balance(500, 900))],
            ..Default::default()
        })
        .await;
        let addrs = vec![t_addr(ADDR_A)];
        let total = address_balance(&snapshot, &addrs)
            .await
            .expect("explorer answers zero, not an error");
        assert_eq!(total.balance.as_u64(), 0);
        assert_eq!(u128::from(total.received), 0);
    }

    /// Wallet side, on the identical input: `None`, never zero, so the adapter
    /// can refuse rather than tell a user their funds are gone.
    #[tokio::test]
    async fn wallet_balance_is_none_when_nothing_is_serviceable() {
        let snapshot = snapshot_with(MockChain {
            tip: None,
            balances: vec![(ADDR_A.to_string(), balance(500, 900))],
            ..Default::default()
        })
        .await;
        let addrs = vec![t_addr(ADDR_A)];
        let answer = wallet_balance(&snapshot, &addrs)
            .await
            .expect("no read failure");
        assert!(answer.is_none());
    }

    fn delta(height: u32, satoshis: i64, addr: &str) -> zaino_primitives::types::AddressDelta {
        use zaino_primitives::types::SignedZatoshis;
        zaino_primitives::types::AddressDelta {
            satoshis: SignedZatoshis::try_new(satoshis).expect("valid delta"),
            txid: zaino_primitives::types::TransactionId::from([7u8; 32]),
            index: 0,
            height: Height::try_from(height).expect("valid height"),
            address: t_addr(addr),
            block_index: Some(1),
        }
    }

    /// A delta with the sort-key positions (`block_index`, `index`) spelled out,
    /// for the tests that exercise tie-breaking rather than just height.
    fn delta_at(
        height: u32,
        block_index: Option<u32>,
        index: u32,
        addr: &str,
    ) -> zaino_primitives::types::AddressDelta {
        use zaino_primitives::types::SignedZatoshis;
        zaino_primitives::types::AddressDelta {
            satoshis: SignedZatoshis::try_new(1).expect("valid delta"),
            txid: zaino_primitives::types::TransactionId::from([7u8; 32]),
            index,
            height: Height::try_from(height).expect("valid height"),
            address: t_addr(addr),
            block_index,
        }
    }

    fn tipped(deltas: Vec<zaino_primitives::types::AddressDelta>) -> MockChain {
        MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(200).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            deltas,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn address_deltas_filter_by_address_and_height() {
        let snapshot = snapshot_with(tipped(vec![
            delta(100, 5, ADDR_A),
            delta(150, -3, ADDR_A),
            delta(150, 9, ADDR_B),
        ]))
        .await;
        let addrs = vec![t_addr(ADDR_A)];
        let answer = address_deltas(
            &snapshot,
            &addrs,
            Some(Height::try_from(120).expect("valid height")),
            Some(Height::try_from(160).expect("valid height")),
        )
        .await
        .expect("deltas");
        assert_eq!(answer.deltas.len(), 1);
        assert_eq!(answer.deltas[0].satoshis.as_i64(), -3);
        assert!(answer.range.is_some(), "a real query reports its range");
    }

    /// `HeightRange` is inclusive, so start == end is a one-block query that
    /// must return that block's delta. This is the boundary a half-open reading
    /// gets wrong, and it gets it wrong silently.
    #[tokio::test]
    async fn a_single_height_delta_range_is_inclusive() {
        let snapshot = snapshot_with(tipped(vec![delta(150, -3, ADDR_A)])).await;
        let addrs = vec![t_addr(ADDR_A)];
        let at = Height::try_from(150).expect("valid height");
        let answer = address_deltas(&snapshot, &addrs, Some(at), Some(at))
            .await
            .expect("deltas");
        assert_eq!(
            answer.deltas.len(),
            1,
            "[150, 150] is one block, not an empty range"
        );
    }

    /// Callers derive these bounds from user-supplied dates, where an empty day
    /// is ordinary. A backwards range is a valid query with an empty answer.
    #[tokio::test]
    async fn a_backwards_delta_range_is_empty_not_an_error() {
        let snapshot = snapshot_with(tipped(vec![delta(100, 5, ADDR_A)])).await;
        let addrs = vec![t_addr(ADDR_A)];
        let answer = address_deltas(
            &snapshot,
            &addrs,
            Some(Height::try_from(900).expect("valid height")),
            Some(Height::try_from(100).expect("valid height")),
        )
        .await
        .expect("a backwards range is a valid query");
        assert!(answer.deltas.is_empty());
        assert!(
            answer.range.is_none(),
            "no query ran, so no range to report"
        );
    }

    /// Explorer policy, matching `address_balance`: nothing serviceable means no
    /// indexed history. Scripted, so a regression that queried a synthesised
    /// range would read the scripted delta and fail.
    #[tokio::test]
    async fn address_deltas_are_empty_when_nothing_is_serviceable() {
        let snapshot = snapshot_with(MockChain {
            tip: None,
            deltas: vec![delta(0, 5, ADDR_A)],
            ..Default::default()
        })
        .await;
        let addrs = vec![t_addr(ADDR_A)];
        let answer = address_deltas(&snapshot, &addrs, None, None)
            .await
            .expect("no coverage is a valid query");
        assert!(answer.deltas.is_empty());
        assert!(answer.range.is_none());
    }

    /// zcashd documents the order as (height, blockindex, index), so all three
    /// keys must break ties, not height alone. The input is scrambled on each
    /// key: out of order by height, and — at one shared height — differing on
    /// `block_index` and then on `index`.
    ///
    /// `block_index` is `Option<u32>` because a source may not supply it; zcashd
    /// documents no order for that case, so our choice is arbitrary. It is pinned
    /// here so it cannot drift silently: Rust's derived `Option` ordering sorts
    /// `None` before any `Some`, so an unknown-position delta sorts ahead of its
    /// same-height siblings.
    #[tokio::test]
    async fn address_deltas_are_ordered_by_height_then_position() {
        let snapshot = snapshot_with(tipped(vec![
            delta_at(150, Some(1), 0, ADDR_A),
            delta_at(100, Some(2), 0, ADDR_A), // out of order by height
            delta_at(150, Some(0), 0, ADDR_A), // same height, lower block_index
            delta_at(150, Some(1), 5, ADDR_A), // same height and block_index, higher index
            delta_at(150, None, 0, ADDR_A),    // unknown position sorts before Some
        ]))
        .await;
        let addrs = vec![t_addr(ADDR_A)];
        let answer = address_deltas(&snapshot, &addrs, None, None)
            .await
            .expect("deltas");
        let order: Vec<(u32, Option<u32>, u32)> = answer
            .deltas
            .iter()
            .map(|d| (u32::from(d.height), d.block_index, d.index))
            .collect();
        assert_eq!(
            order,
            vec![
                (100, Some(2), 0),
                (150, None, 0),
                (150, Some(0), 0),
                (150, Some(1), 0),
                (150, Some(1), 5),
            ]
        );
    }
}
