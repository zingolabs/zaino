//! From the walked bytes to the domain transaction.

use zaino_primitives::types::{
    CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier, OrchardAction, OrchardData,
    SaplingData, SaplingOutput, SaplingSpend, Script, SignedZatoshis, Transaction, TransactionId,
    TransparentData, TransparentInput, TransparentOutput, Zatoshis,
};

use crate::error::DecodeError;
use crate::transaction::{RawOrchardBundle, RawSapling, RawTransaction};
use crate::txid::transaction_id;

pub(crate) fn project_transaction(tx: &RawTransaction<'_>) -> Result<Transaction, DecodeError> {
    Ok(Transaction {
        txid: TransactionId::from(transaction_id(tx)),
        transparent: transparent(tx)?,
        sapling: sapling(&tx.sapling)?,
        orchard: orchard_shaped(tx.orchard.as_ref())?,
        ironwood: orchard_shaped(tx.ironwood.as_ref())?,
    })
}

/// Outpoints spent and outputs created. A coinbase input spends nothing, so
/// it is not an input here.
fn transparent(tx: &RawTransaction<'_>) -> Result<TransparentData, DecodeError> {
    let inputs = tx
        .inputs
        .iter()
        .filter(|input| !input.is_coinbase())
        .map(|input| TransparentInput {
            prev_txid: TransactionId::from(input.prevout_hash),
            prev_index: input.prevout_index,
        })
        .collect();
    let outputs = tx
        .outputs
        .iter()
        .map(|output| {
            // Encoded as a signed amount; a negative one reads as a value past
            // the supply and is rejected as such.
            let value = u64::from_le_bytes(output.value);
            Ok(TransparentOutput {
                value: Zatoshis::new(value)?,
                script: Script::new(output.script.to_vec()),
            })
        })
        .collect::<Result<Vec<_>, DecodeError>>()?;
    Ok(TransparentData { inputs, outputs })
}

fn sapling(sapling: &RawSapling<'_>) -> Result<SaplingData, DecodeError> {
    Ok(SaplingData {
        spends: sapling
            .spends
            .iter()
            .map(|spend| SaplingSpend {
                nullifier: Nullifier::from(spend.nullifier),
            })
            .collect(),
        outputs: sapling
            .outputs
            .iter()
            .map(|output| SaplingOutput {
                cmu: NoteCommitment::from(output.cmu),
                ephemeral_key: EphemeralKey::from(output.ephemeral_key),
                enc_ciphertext: compact_head(output.enc_ciphertext),
            })
            .collect(),
        value_balance: SignedZatoshis::try_new(sapling.value_balance)?,
    })
}

/// Orchard and Ironwood share one shape, so one projection.
fn orchard_shaped(bundle: Option<&RawOrchardBundle<'_>>) -> Result<OrchardData, DecodeError> {
    let Some(bundle) = bundle else {
        return Ok(OrchardData::default());
    };
    Ok(OrchardData {
        actions: bundle
            .actions
            .iter()
            .map(|action| OrchardAction {
                nullifier: Nullifier::from(action.nullifier),
                cmx: NoteCommitment::from(action.cmx),
                ephemeral_key: EphemeralKey::from(action.ephemeral_key),
                enc_ciphertext: compact_head(action.enc_ciphertext),
            })
            .collect(),
        value_balance: SignedZatoshis::try_new(bundle.value_balance)?,
    })
}

/// The scanning prefix of a full note ciphertext. The walk took the full
/// ciphertext by its fixed size, so the prefix is always there.
fn compact_head(enc_ciphertext: &[u8]) -> CompactCiphertext {
    let mut head = [0u8; CompactCiphertext::LENGTH];
    head.copy_from_slice(&enc_ciphertext[..CompactCiphertext::LENGTH]);
    CompactCiphertext::from(head)
}
