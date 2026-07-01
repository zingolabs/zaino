//! Holds code used to build test vector data for unit tests. These tests should not be run by default or in CI.

use anyhow::Context;
use wire_serialized_transaction_test_data::transactions::get_test_vectors;
use zaino_fetch::chain::transaction::FullTransaction;
use zaino_fetch::chain::utils::ParseFromSlice;

#[tokio::test(flavor = "multi_thread")]
async fn pre_v4_txs_parsing() -> anyhow::Result<()> {
    let test_vectors = get_test_vectors();

    for (i, test_vector) in test_vectors.iter().filter(|v| v.version < 4).enumerate() {
        let description = test_vector.description;
        let version = test_vector.version;
        let raw_tx = test_vector.tx.clone();
        let txid = test_vector.txid;
        // todo!: add an 'is_coinbase' method to the transaction struct to check thid
        let _is_coinbase = test_vector.is_coinbase;
        let has_sapling = test_vector.has_sapling;
        let has_orchard = test_vector.has_orchard;
        let transparent_inputs = test_vector.transparent_inputs;
        let transparent_outputs = test_vector.transparent_outputs;

        let deserialized_tx =
            FullTransaction::parse_from_slice(&raw_tx, Some(vec![txid.to_vec()]), None)
                .with_context(|| {
                    format!("Failed to deserialize transaction with description: {description:?}")
                })?;

        let tx = deserialized_tx.1;

        assert_eq!(
            tx.version(),
            version,
            "Version mismatch for transaction #{i} ({description})"
        );
        assert_eq!(
            tx.tx_id(),
            txid,
            "TXID mismatch for transaction #{i} ({description})"
        );
        // Check Sapling spends (v4+ transactions)
        if version >= 4 {
            assert_eq!(
                !tx.shielded_spends().is_empty(),
                has_sapling != 0,
                "Sapling spends mismatch for transaction #{i} ({description})"
            );
        } else {
            // v1-v3 transactions should not have Sapling spends
            assert!(
                tx.shielded_spends().is_empty(),
                "Transaction #{i} ({description}) version {version} should not have Sapling spends"
            );
        }

        // Check Orchard actions (v5+ transactions)
        if version >= 5 {
            assert_eq!(
                !tx.orchard_actions().is_empty(),
                has_orchard != 0,
                "Orchard actions mismatch for transaction #{i} ({description})"
            );
        } else {
            // v1-v4 transactions should not have Orchard actions
            assert!(
                tx.orchard_actions().is_empty(),
                "Transaction #{i} ({description}) version {version} should not have Orchard actions"
            );
        }
        assert_eq!(
            !tx.transparent_inputs().is_empty(),
            transparent_inputs > 0,
            "Transparent inputs presence mismatch for transaction #{i} ({description})"
        );
        assert_eq!(
            !tx.transparent_outputs().is_empty(),
            transparent_outputs > 0,
            "Transparent outputs presence mismatch for transaction #{i} ({description})"
        );

        // dbg!(tx);
    }
    Ok(())
}
