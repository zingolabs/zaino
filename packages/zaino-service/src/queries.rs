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

use zaino_primitives::types::{
    AddressBalance, HeightRange, TransparentAddress, Zatoshis, ZatoshisFlowSum,
};

use crate::error::AddressReadError;
use crate::reads::AddressRead;
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
pub async fn total_balance<S: AddressRead>(
    snapshot: &S,
    addrs: &[TransparentAddress],
    range: HeightRange,
) -> Result<AddressBalance, AddressReadError> {
    let mut balances = Vec::with_capacity(addrs.len());
    let mut received = ZatoshisFlowSum::from_summed(0);
    for addr in addrs {
        let read = snapshot.balance(addr, range).await?;
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

#[cfg(test)]
mod tests {
    use super::{address_balance, serviceable_range, total_balance, wallet_balance};
    use crate::testing::{MockChain, MockIndexerService};
    use crate::TakeSnapshot;
    use zaino_primitives::types::{
        AddressBalance, BlockHash, BlockRef, Height, TransparentAddress, Zatoshis, ZatoshisFlowSum,
    };

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
                ("t1a".to_string(), balance(500, 900)),
                ("t1b".to_string(), balance(250, 400)),
            ],
            ..Default::default()
        })
        .await;
        let range = serviceable_range(&snapshot).expect("coverage");
        let addrs = vec![
            TransparentAddress::new("t1a".to_string()),
            TransparentAddress::new("t1b".to_string()),
        ];
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
            balances: vec![("t1a".to_string(), balance(500, 900))],
            ..Default::default()
        })
        .await;
        let addrs = vec![TransparentAddress::new("t1a".to_string())];
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
            balances: vec![("t1a".to_string(), balance(500, 900))],
            ..Default::default()
        })
        .await;
        let addrs = vec![TransparentAddress::new("t1a".to_string())];
        let answer = wallet_balance(&snapshot, &addrs)
            .await
            .expect("no read failure");
        assert!(answer.is_none());
    }
}
