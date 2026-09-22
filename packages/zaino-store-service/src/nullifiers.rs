//! The nullifiers-only projection of a compact block.
//!
//! `GetBlockNullifiers` serves the same compact block as `GetBlock`, reduced to
//! just the spend markers: a wallet that only needs to detect whether its notes
//! were spent downloads these instead of full compact blocks. So the projection
//! keeps every nullifier and drops everything that identifies an *output* — a
//! local transform over the compact block the store already produces, not a
//! separate index.

use zaino_core::{CompactBlock, CompactCiphertext, OrchardAction};

/// Reduce a compact block to its spend markers: keep the sapling nullifiers and
/// each shielded action's nullifier; clear all outputs (transparent, sapling)
/// and the non-nullifier fields of each action. Transparent inputs are dropped
/// too — transparent spends are detected through the address reads, not here.
pub(crate) fn strip_to_nullifiers(mut block: CompactBlock) -> CompactBlock {
    for tx in &mut block.transactions {
        tx.transparent_inputs.clear();
        tx.transparent_outputs.clear();
        tx.sapling_outputs.clear();
        tx.orchard_actions = tx.orchard_actions.iter().map(nullifier_only).collect();
        tx.ironwood_actions = tx.ironwood_actions.iter().map(nullifier_only).collect();
        // `sapling_nullifiers` is already spend-only; kept as is.
    }
    block
}

/// An action carrying only its nullifier — the commitment, ephemeral key, and
/// ciphertext (all output material) zeroed.
fn nullifier_only(action: &OrchardAction) -> OrchardAction {
    OrchardAction {
        nullifier: action.nullifier,
        cmx: [0u8; 32].into(),
        ephemeral_key: [0u8; 32].into(),
        enc_ciphertext: CompactCiphertext::from([0u8; 52]),
    }
}

#[cfg(test)]
mod tests {
    use super::strip_to_nullifiers;
    use zaino_core::{
        ChainMetadata, CompactBlock, CompactCiphertext, CompactDifficulty, OrchardAction,
        PreIndexCompactTx,
    };

    fn action(tag: u8) -> OrchardAction {
        OrchardAction {
            nullifier: [tag; 32].into(),
            cmx: [tag; 32].into(),
            ephemeral_key: [tag; 32].into(),
            enc_ciphertext: CompactCiphertext::from([tag; 52]),
        }
    }

    #[test]
    fn keeps_every_nullifier_and_clears_every_output() {
        let tx = PreIndexCompactTx {
            txid: [1u8; 32].into(),
            transparent_inputs: vec![],
            transparent_outputs: vec![],
            sapling_nullifiers: vec![[7u8; 32].into()],
            sapling_outputs: vec![],
            orchard_actions: vec![action(9)],
            ironwood_actions: vec![action(3)],
        };
        let block = CompactBlock {
            hash: [0u8; 32].into(),
            prev_hash: [0u8; 32].into(),
            height: 0,
            time: 0,
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
            transactions: vec![tx],
            chain_metadata: ChainMetadata::ZERO,
        };

        let stripped = strip_to_nullifiers(block);
        let out = &stripped.transactions[0];

        // Spend markers survive.
        assert_eq!(out.sapling_nullifiers, vec![[7u8; 32].into()]);
        assert_eq!(out.orchard_actions[0].nullifier, action(9).nullifier);
        assert_eq!(out.ironwood_actions[0].nullifier, action(3).nullifier);
        // Output material is gone.
        assert!(out.sapling_outputs.is_empty());
        assert_eq!(out.orchard_actions[0].cmx, [0u8; 32].into());
        assert_eq!(
            out.orchard_actions[0].enc_ciphertext,
            CompactCiphertext::from([0u8; 52])
        );
    }
}
