//! The transparent-address query responses: `getaddressbalance`,
//! `getaddressutxos`.
//!
//! Both reuse Zebra's own types, so this module holds only the conversions from
//! the domain.

use zaino_primitives::types::{AddressBalance as DomainAddressBalance, Utxo};
use zebra_rpc::methods::{AddressBalance, GetAddressUtxos};

/// A UTXO whose address the wire type cannot represent.
///
/// A [`TransparentAddress`](zaino_primitives::types::TransparentAddress) is a
/// validated transparent address, but Zebra's `GetAddressUtxos` holds a parsed
/// `transparent::Address` of its own, so rendering re-parses with a second
/// implementation, and that step is typed as fallible.
///
/// In practice it cannot fail: our constructor already accepted the string as a
/// transparent address, and Zebra parses the same set. It is reported rather
/// than asserted because the two parsers are separate implementations, and a
/// disagreement between them is a reason to fail the query, not to panic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("utxo address is not a valid transparent address: {0}")]
pub struct UnrenderableUtxoAddress(String);

/// A received total the wire type cannot represent.
///
/// The domain carries lifetime receipts as a flow sum over a `u128`; the
/// `getaddressbalance` response carries `received` as a `u64`. Every backend
/// delivers the total already summed into a `u64`, so a value past that width
/// cannot arise in practice. It is reported rather than asserted because the
/// alternative is a panic on a value that came from outside this process.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("received total {0} exceeds the wire field's u64 range")]
pub struct UnrenderableReceivedTotal(u128);

/// Renders the domain type as the `getaddressbalance` response.
pub fn address_balance_from_domain(
    balance: DomainAddressBalance,
) -> Result<AddressBalance, UnrenderableReceivedTotal> {
    let received = u128::from(balance.received);
    let received = u64::try_from(received).map_err(|_| UnrenderableReceivedTotal(received))?;
    Ok(AddressBalance::new(u64::from(balance.balance), received))
}

/// Renders the domain UTXOs as the `getaddressutxos` response.
///
/// Order is the source's and is not re-sorted here.
pub fn address_utxos_from_domain(
    utxos: Vec<Utxo>,
) -> Result<Vec<GetAddressUtxos>, UnrenderableUtxoAddress> {
    utxos
        .into_iter()
        .map(|utxo| {
            Ok(GetAddressUtxos::new(
                utxo.address
                    .as_str()
                    .parse()
                    .map_err(|e| UnrenderableUtxoAddress(format!("{e}")))?,
                zebra_chain::transaction::Hash::from(<[u8; 32]>::from(utxo.txid)),
                zebra_chain::transparent::OutputIndex::from_index(utxo.output_index),
                zebra_chain::transparent::Script::new(&Vec::<u8>::from(utxo.script)),
                u64::from(utxo.satoshis),
                zebra_chain::block::Height(utxo.height.into()),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{
        Height, Script, TransactionId, TransparentAddress, Zatoshis, ZatoshisFlowSum,
    };

    /// Asymmetric under reversal, so a missing or doubled byte-reversal shows up.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    /// A real testnet P2PKH address; an invented one would fail the checksum.
    const ADDRESS: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";

    fn utxo() -> Utxo {
        Utxo {
            address: TransparentAddress::try_new(ADDRESS).expect("valid testnet address"),
            txid: TransactionId::from(ASYMMETRIC),
            output_index: 2,
            script: Script::from(vec![0x76, 0xa9]),
            satoshis: Zatoshis::new(50_000).unwrap(),
            height: Height::try_from(1_234u32).unwrap(),
        }
    }

    /// Both figures are integer zatoshis on this interface, not ZEC — unlike
    /// `getblocksubsidy` and `getinfo`, which are ZEC. The distinction is
    /// per-method, so it is pinned per method.
    #[test]
    fn balance_is_reported_in_zatoshis() {
        let json = serde_json::to_value(
            address_balance_from_domain(DomainAddressBalance {
                balance: Zatoshis::new(150_000_000).unwrap(),
                received: ZatoshisFlowSum::from_summed(200_000_000),
            })
            .expect("a u64-summed received total renders"),
        )
        .unwrap();

        assert_eq!(json["balance"], 150_000_000u64);
        assert_eq!(json["received"], 200_000_000u64);
    }

    /// `received` is a flow: a lifetime total past the money supply is
    /// legitimate data and round-trips to the wire unchanged.
    #[test]
    fn a_received_total_past_the_supply_renders() {
        let json = serde_json::to_value(
            address_balance_from_domain(DomainAddressBalance {
                balance: Zatoshis::ZERO,
                received: ZatoshisFlowSum::from_summed(u64::MAX),
            })
            .expect("any u64-summed received total renders"),
        )
        .unwrap();

        assert_eq!(json["received"], u64::MAX);
    }

    #[test]
    fn utxo_renders_legacy_field_names_and_a_display_order_txid() {
        let rendered = address_utxos_from_domain(vec![utxo()]).expect("address is valid");
        let json = serde_json::to_value(&rendered).unwrap();

        let mut display_order = ASYMMETRIC;
        display_order.reverse();

        assert_eq!(json[0]["address"], ADDRESS);
        assert_eq!(json[0]["txid"], hex::encode(display_order));
        assert_eq!(json[0]["outputIndex"], 2);
        assert_eq!(json[0]["satoshis"], 50_000u64);
        assert_eq!(json[0]["height"], 1_234);
    }

    /// A malformed address is now rejected where it is *constructed*, not where
    /// it is rendered: [`TransparentAddress`] cannot hold a non-address, so the
    /// rendering path never receives one. What was once a render-time error
    /// check is now a construction-time invariant, so the rejection is asserted
    /// at the boundary that enforces it.
    #[test]
    fn a_malformed_address_is_rejected_at_construction() {
        assert!(TransparentAddress::try_new("not an address").is_err());
    }
}
