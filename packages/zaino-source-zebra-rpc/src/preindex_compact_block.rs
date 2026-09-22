//! Client of the validator's `getpreindexcompactblock` RPC.
//!
//! The response mirrors the fork's `GetPreindexCompactBlockResponse` (hex byte
//! fields). It is reconstructed into the fork's
//! [`CompactBlock`](zebra_chain::transaction::compact::CompactBlock) so the same
//! [`pre_index_compact_block_from_zebra`](zaino_convert_zebra::pre_index_compact_block_from_zebra)
//! conversion serves both the RPC and in-process (ReadState) compact paths — one
//! mapping, no drift.

use serde::Deserialize;

use zebra_chain::{
    block,
    serialization::ZcashDeserializeInto,
    transaction::{
        self,
        compact::{
            CompactBlock, CompactOrchardAction, CompactOutput, CompactSaplingOutput,
            CompactTransaction,
        },
    },
    transparent,
};

use crate::parse::ParseError;

/// The validator's `getpreindexcompactblock` response — field-for-field the
/// fork's DTO, with byte fields as hex strings.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PreindexCompactBlockResponse {
    hash: String,
    height: u32,
    header: String,
    transactions: Vec<PreindexCompactTransaction>,
}

#[derive(Debug, Clone, Deserialize)]
struct PreindexCompactTransaction {
    txid: String,
    transparent_inputs: Vec<PreindexCompactOutpoint>,
    transparent_outputs: Vec<PreindexCompactOutput>,
    sapling_nullifiers: Vec<String>,
    sapling_outputs: Vec<PreindexCompactSaplingOutput>,
    orchard_actions: Vec<PreindexCompactOrchardAction>,
    ironwood_actions: Vec<PreindexCompactOrchardAction>,
}

#[derive(Debug, Clone, Deserialize)]
struct PreindexCompactOutpoint {
    txid: String,
    index: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct PreindexCompactOutput {
    value: u64,
    script: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PreindexCompactSaplingOutput {
    cmu: String,
    ephemeral_key: String,
    enc_ciphertext_head: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PreindexCompactOrchardAction {
    nullifier: String,
    cmx: String,
    ephemeral_key: String,
    enc_ciphertext_head: String,
}

impl PreindexCompactBlockResponse {
    /// Reconstruct the fork's compact block from the wire form.
    pub(crate) fn into_zebra_compact(self) -> Result<CompactBlock, ParseError> {
        let header: block::Header = hex_bytes(&self.header)?
            .zcash_deserialize_into()
            .map_err(|e| ParseError::Deserialize(format!("compact block header: {e}")))?;
        let hash: block::Hash = self
            .hash
            .parse()
            .map_err(|e| ParseError::Deserialize(format!("compact block hash: {e}")))?;
        let transactions = self
            .transactions
            .into_iter()
            .map(PreindexCompactTransaction::into_zebra)
            .collect::<Result<Vec<_>, ParseError>>()?;
        Ok(CompactBlock {
            header,
            hash,
            height: block::Height(self.height),
            transactions,
        })
    }
}

impl PreindexCompactTransaction {
    fn into_zebra(self) -> Result<CompactTransaction, ParseError> {
        let txid: transaction::Hash = self
            .txid
            .parse()
            .map_err(|e| ParseError::Deserialize(format!("compact tx id: {e}")))?;
        let transparent_inputs = self
            .transparent_inputs
            .into_iter()
            .map(|outpoint| {
                Ok(transparent::OutPoint {
                    hash: outpoint.txid.parse().map_err(|e| {
                        ParseError::Deserialize(format!("compact outpoint id: {e}"))
                    })?,
                    index: outpoint.index,
                })
            })
            .collect::<Result<Vec<_>, ParseError>>()?;
        let transparent_outputs = self
            .transparent_outputs
            .into_iter()
            .map(|output| {
                Ok(CompactOutput {
                    value: output.value,
                    script: hex_bytes(&output.script)?,
                })
            })
            .collect::<Result<Vec<_>, ParseError>>()?;
        let sapling_nullifiers = self
            .sapling_nullifiers
            .iter()
            .map(|nullifier| hex_array::<32>(nullifier))
            .collect::<Result<Vec<_>, ParseError>>()?;
        let sapling_outputs = self
            .sapling_outputs
            .into_iter()
            .map(|output| {
                Ok(CompactSaplingOutput {
                    cmu: hex_array::<32>(&output.cmu)?,
                    ephemeral_key: hex_array::<32>(&output.ephemeral_key)?,
                    enc_ciphertext_head: hex_array::<52>(&output.enc_ciphertext_head)?,
                })
            })
            .collect::<Result<Vec<_>, ParseError>>()?;
        let orchard_actions = actions_into_zebra(self.orchard_actions)?;
        let ironwood_actions = actions_into_zebra(self.ironwood_actions)?;
        Ok(CompactTransaction {
            txid,
            transparent_inputs,
            transparent_outputs,
            sapling_nullifiers,
            sapling_outputs,
            orchard_actions,
            ironwood_actions,
        })
    }
}

/// Reconstruct a pool's compact actions (Orchard or Ironwood — same shape) from
/// their wire hex fields.
fn actions_into_zebra(
    actions: Vec<PreindexCompactOrchardAction>,
) -> Result<Vec<CompactOrchardAction>, ParseError> {
    actions
        .into_iter()
        .map(|action| {
            Ok(CompactOrchardAction {
                nullifier: hex_array::<32>(&action.nullifier)?,
                cmx: hex_array::<32>(&action.cmx)?,
                ephemeral_key: hex_array::<32>(&action.ephemeral_key)?,
                enc_ciphertext_head: hex_array::<52>(&action.enc_ciphertext_head)?,
            })
        })
        .collect()
}

/// Decode a hex string to bytes, naming the failure for the seam.
fn hex_bytes(hex: &str) -> Result<Vec<u8>, ParseError> {
    hex::decode(hex).map_err(|e| ParseError::Deserialize(format!("hex: {e}")))
}

/// Decode a hex string to a fixed-size byte array.
fn hex_array<const N: usize>(hex: &str) -> Result<[u8; N], ParseError> {
    let bytes = hex_bytes(hex)?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| ParseError::Deserialize(format!("expected {N} bytes, got {}", bytes.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transaction's hex fields reconstruct into the fork compact type in the
    /// right slots and byte lengths — the wire mapping the RPC path depends on.
    #[test]
    fn transaction_reconstructs_from_wire_fields() {
        let json = format!(
            r#"{{
                "txid": "{txid}",
                "transparent_inputs": [{{ "txid": "{prev}", "index": 7 }}],
                "transparent_outputs": [{{ "value": 12345, "script": "deadbeef" }}],
                "sapling_nullifiers": ["{nf}"],
                "sapling_outputs": [{{ "cmu": "{cmu}", "ephemeral_key": "{epk}", "enc_ciphertext_head": "{ct}" }}],
                "orchard_actions": [{{ "nullifier": "{onf}", "cmx": "{cmx}", "ephemeral_key": "{oepk}", "enc_ciphertext_head": "{oct}" }}],
                "ironwood_actions": [{{ "nullifier": "{inf}", "cmx": "{icmx}", "ephemeral_key": "{iepk}", "enc_ciphertext_head": "{ict}" }}]
            }}"#,
            txid = "11".repeat(32),
            prev = "22".repeat(32),
            nf = "33".repeat(32),
            cmu = "44".repeat(32),
            epk = "55".repeat(32),
            ct = "66".repeat(52),
            onf = "77".repeat(32),
            cmx = "88".repeat(32),
            oepk = "99".repeat(32),
            oct = "aa".repeat(52),
            inf = "bb".repeat(32),
            icmx = "cc".repeat(32),
            iepk = "dd".repeat(32),
            ict = "ee".repeat(52),
        );

        let wire: PreindexCompactTransaction =
            serde_json::from_str(&json).expect("wire tx deserializes");
        let tx = wire.into_zebra().expect("reconstructs");

        assert_eq!(tx.txid.to_string(), "11".repeat(32));
        assert_eq!(tx.transparent_inputs[0].hash.to_string(), "22".repeat(32));
        assert_eq!(tx.transparent_inputs[0].index, 7);
        assert_eq!(tx.transparent_outputs[0].value, 12345);
        assert_eq!(
            tx.transparent_outputs[0].script,
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(tx.sapling_nullifiers[0], [0x33; 32]);
        assert_eq!(tx.sapling_outputs[0].cmu, [0x44; 32]);
        assert_eq!(tx.sapling_outputs[0].ephemeral_key, [0x55; 32]);
        assert_eq!(tx.sapling_outputs[0].enc_ciphertext_head, [0x66; 52]);
        assert_eq!(tx.orchard_actions[0].nullifier, [0x77; 32]);
        assert_eq!(tx.orchard_actions[0].cmx, [0x88; 32]);
        assert_eq!(tx.orchard_actions[0].ephemeral_key, [0x99; 32]);
        assert_eq!(tx.orchard_actions[0].enc_ciphertext_head, [0xaa; 52]);
        assert_eq!(tx.ironwood_actions[0].nullifier, [0xbb; 32]);
        assert_eq!(tx.ironwood_actions[0].cmx, [0xcc; 32]);
        assert_eq!(tx.ironwood_actions[0].ephemeral_key, [0xdd; 32]);
        assert_eq!(tx.ironwood_actions[0].enc_ciphertext_head, [0xee; 52]);
    }

    #[test]
    fn wrong_length_hex_is_rejected() {
        assert!(hex_array::<32>("00").is_err());
        assert!(hex_array::<32>(&"11".repeat(32)).is_ok());
    }
}
