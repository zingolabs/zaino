//! Consensus rules a transaction's own bytes, the next block and Zaino's UTXO set decide
//! (`docs/architecture.md` §5): a submission's precheck, and a peer quorum's admission
//!
//! - consensus only: fee floors are node policy (zebrad 1,000 / zakura 400 zatoshis per action),
//!   carried by the peer quorum, never second-guessed here
//! - never run: proofs, signatures, scripts, coinbase maturity (the quorum's validators ran them)

use zaino_primitives::types::{
    ConsensusBranchId, Fee, FeeError, Height, TransactionId, UpgradeSchedule,
};
use zaino_source::{decode_transaction, prepare_transaction, Prepared};
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

/// What a transaction must be valid in: the block above the verified best
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NextBlock {
    pub(crate) height: Height,
    pub(crate) branch: ConsensusBranchId,
}

impl NextBlock {
    pub(crate) fn above(best: Height, schedule: &UpgradeSchedule) -> Self {
        let height = best.next();
        Self { height, branch: schedule.branch_at(height) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refused {
    #[error("malformed transaction: {0}")]
    Malformed(String),
    #[error("a coinbase transaction (never relayed)")]
    Coinbase,
    #[error("tx-expiring-soon: expiry height {expiry} is below the next block {next}")]
    Expired { expiry: u32, next: u32 },
    #[error("built for consensus branch {built}, the next block's is {next}")]
    Branch { built: ConsensusBranchId, next: ConsensusBranchId },
    #[error(transparent)]
    Fee(#[from] FeeError),
    #[error("prevout {txid}:{vout} neither in the served UTXO set nor a held transaction's")]
    UnknownPrevout { txid: TransactionId, vout: u32 },
}

impl Refused {
    /// A later round may pass it (the served tip or the mempool catching up to its parent)
    pub(crate) fn retried(&self) -> bool {
        matches!(self, Self::UnknownPrevout { .. })
    }
}

/// Expiry (ZIP 203: invalid in any block above it) + the branch v5+ signs for
pub(crate) fn valid_in(prepared: &Prepared, next: NextBlock) -> Result<(), Refused> {
    if let Some(expiry) = prepared.expiry_height.filter(|&expiry| next.height > expiry) {
        let (expiry, next) = (u32::from(expiry), u32::from(next.height));
        return Err(Refused::Expired { expiry, next });
    }
    match prepared.branch.filter(|&built| built != next.branch) {
        Some(built) => Err(Refused::Branch { built, next: next.branch }),
        None => Ok(()),
    }
}

/// Every check a peer quorum's transaction must pass here → its txid + fee
///
/// - `prevout` = an outpoint's value, unspent (served UTXO set, else a held transaction's output)
pub(crate) fn admissible(
    raw: &[u8],
    next: NextBlock,
    prevout: impl Fn(&OutPoint) -> Option<Zatoshis>,
) -> Result<(TransactionId, Zatoshis), Refused> {
    let malformed = |cause: zaino_source::DecodeError| Refused::Malformed(cause.to_string());
    let prepared = prepare_transaction(raw).map_err(malformed)?;
    valid_in(&prepared, next)?;
    let tx = decode_transaction(raw).map_err(malformed)?;
    if tx.transparent.coinbase {
        return Err(Refused::Coinbase);
    }
    let mut spent = Zatoshis::ZERO;
    for input in &tx.transparent.inputs {
        let unknown = || Refused::UnknownPrevout {
            txid: TransactionId::from(*input.hash()),
            vout: input.n(),
        };
        let value = prevout(input).ok_or_else(unknown)?;
        spent = (spent + value).ok_or(Refused::Fee(FeeError::Overflow))?;
    }
    match tx.fee(spent)? {
        Fee::Paid(fee) => Ok((tx.txid, fee)),
        Fee::Coinbase => Err(Refused::Coinbase),
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use zaino_primitives::testing::{h, p2pkh};
    use zaino_source::testing::{raw_transaction, raw_transparent, raw_v5};
    use zcash_protocol::consensus::{BranchId, NetworkType};

    use super::*;

    /// Real bytes against the block above mainnet 3,428,142 (NU6.3's first): the spend of a known
    /// prevout passes with its fee; each rule refuses by name, an unknown prevout alone retried
    #[test]
    fn each_consensus_rule_refuses_by_name_and_only_an_unknown_prevout_is_retried() {
        let mainnet = UpgradeSchedule::compiled(NetworkType::Main).expect("compiled");
        let next = NextBlock::above(h(3_428_142), &mainnet);
        let branch = ConsensusBranchId::from(BranchId::Nu6_3);
        assert_eq!(next, NextBlock { height: h(3_428_143), branch });

        let alice = p2pkh([0xaa; 20]);
        let known = OutPoint::new([1; 32], 0);
        let unknown = OutPoint::new([2; 32], 3);
        let coinbase_input = OutPoint::new([0; 32], u32::MAX);
        let zats = |n: u64| Zatoshis::from_u64(n).expect("zats");
        let utxos = |at: &OutPoint| (*at == known).then(|| zats(10_000));

        let (paying, pays) = raw_transparent(slice::from_ref(&known), &[(&alice, 9_000)]);
        let (_, overpays) = raw_transparent(slice::from_ref(&known), &[(&alice, 11_000)]);
        let (_, orphan) = raw_transparent(&[unknown], &[(&alice, 1)]);
        let (_, coinbase) = raw_transparent(&[coinbase_input], &[(&alice, 1)]);
        let (lasts, lasting) = raw_transaction(1, 3_428_143);
        let (_, expired) = raw_transaction(2, 3_428_142);
        let (current, nu6_3) = raw_v5(BranchId::Nu6_3, 3, 0);
        let (_, nu6_2) = raw_v5(BranchId::Nu6_2, 4, 0);
        type Admitted = Result<(TransactionId, Zatoshis), Refused>;
        let malformed =
            Refused::Malformed("malformed consensus bytes: failed to fill whole buffer".into());
        let rows: [(&str, &[u8], Admitted, bool); 9] = [
            ("pays 9,000 of 10,000", &pays, Ok((paying, zats(1_000))), false),
            ("pays 11,000 of 10,000", &overpays, Err(Refused::Fee(FeeError::Negative)), false),
            (
                "spends an unknown prevout",
                &orphan,
                Err(Refused::UnknownPrevout { txid: TransactionId::from([2; 32]), vout: 3 }),
                true,
            ),
            ("a coinbase", &coinbase, Err(Refused::Coinbase), false),
            ("expires in the next block", &lasting, Ok((lasts, zats(0))), false),
            (
                "expired below it",
                &expired,
                Err(Refused::Expired { expiry: 3_428_142, next: 3_428_143 }),
                false,
            ),
            ("signed for NU6.3", &nu6_3, Ok((current, zats(0))), false),
            (
                "signed for NU6.2",
                &nu6_2,
                Err(Refused::Branch { built: BranchId::Nu6_2.into(), next: next.branch }),
                false,
            ),
            ("one byte short", &pays[..pays.len() - 1], Err(malformed), false),
        ];
        for (row, raw, expected, retried) in rows {
            let got = admissible(raw, next, utxos);
            assert_eq!(got, expected, "{row}");
            assert_eq!(got.err().is_some_and(|refused| refused.retried()), retried, "{row}");
        }
    }
}
