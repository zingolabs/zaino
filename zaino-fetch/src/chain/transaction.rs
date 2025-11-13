//! Transaction fetching and deserialization functionality.

use crate::chain::{
    error::ParseError,
    utils::{read_bytes, read_i64, read_u32, read_u64, skip_bytes, CompactSize, ParseFromSlice},
};
use std::io::Cursor;
use zaino_proto::proto::compact_formats::{
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTx,
};

/// Txin format as described in <https://en.bitcoin.it/wiki/Transaction>
#[derive(Debug, Clone)]
pub struct TxIn {
    // PrevTxHash - Size\[bytes\]: 32
    prev_txid: Vec<u8>,
    // PrevTxOutIndex - Size\[bytes\]: 4
    prev_index: u32,
    /// CompactSize-prefixed, could be a pubkey or a script
    ///
    /// Size\[bytes\]: CompactSize
    script_sig: Vec<u8>,
    // SequenceNumber \[IGNORED\] - Size\[bytes\]: 4
}

impl TxIn {
    fn into_inner(self) -> (Vec<u8>, u32, Vec<u8>) {
        (self.prev_txid, self.prev_index, self.script_sig)
    }
}

impl ParseFromSlice for TxIn {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        if txid.is_some() {
            return Err(ParseError::InvalidData(
                "txid must be None for TxIn::parse_from_slice".to_string(),
            ));
        }
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for TxIn::parse_from_slice".to_string(),
            ));
        }
        let mut cursor = Cursor::new(data);

        let prev_txid = read_bytes(&mut cursor, 32, "Error reading TxIn::PrevTxHash")?;
        let prev_index = read_u32(&mut cursor, "Error reading TxIn::PrevTxOutIndex")?;
        let script_sig = {
            let compact_length = CompactSize::read(&mut cursor)?;
            read_bytes(
                &mut cursor,
                compact_length as usize,
                "Error reading TxIn::ScriptSig",
            )?
        };
        skip_bytes(&mut cursor, 4, "Error skipping TxIn::SequenceNumber")?;

        Ok((
            &data[cursor.position() as usize..],
            TxIn {
                prev_txid,
                prev_index,
                script_sig,
            },
        ))
    }
}

/// Txout format as described in <https://en.bitcoin.it/wiki/Transaction>
#[derive(Debug, Clone)]
pub struct TxOut {
    /// Non-negative int giving the number of zatoshis to be transferred
    ///
    /// Size\[bytes\]: 8
    value: u64,
    // Script - Size\[bytes\]: CompactSize
    script_hash: Vec<u8>,
}

impl TxOut {
    fn into_inner(self) -> (u64, Vec<u8>) {
        (self.value, self.script_hash)
    }
}

impl ParseFromSlice for TxOut {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        if txid.is_some() {
            return Err(ParseError::InvalidData(
                "txid must be None for TxOut::parse_from_slice".to_string(),
            ));
        }
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for TxOut::parse_from_slice".to_string(),
            ));
        }
        let mut cursor = Cursor::new(data);

        let value = read_u64(&mut cursor, "Error TxOut::reading Value")?;
        let script_hash = {
            let compact_length = CompactSize::read(&mut cursor)?;
            read_bytes(
                &mut cursor,
                compact_length as usize,
                "Error reading TxOut::ScriptHash",
            )?
        };

        Ok((
            &data[cursor.position() as usize..],
            TxOut { script_hash, value },
        ))
    }
}

#[allow(clippy::type_complexity)]
fn parse_transparent(data: &[u8]) -> Result<(&[u8], Vec<TxIn>, Vec<TxOut>), ParseError> {
    let mut cursor = Cursor::new(data);

    let tx_in_count = CompactSize::read(&mut cursor)?;
    let mut tx_ins = Vec::with_capacity(tx_in_count as usize);
    for _ in 0..tx_in_count {
        let (remaining_data, tx_in) =
            TxIn::parse_from_slice(&data[cursor.position() as usize..], None, None)?;
        tx_ins.push(tx_in);
        cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
    }
    let tx_out_count = CompactSize::read(&mut cursor)?;
    let mut tx_outs = Vec::with_capacity(tx_out_count as usize);
    for _ in 0..tx_out_count {
        let (remaining_data, tx_out) =
            TxOut::parse_from_slice(&data[cursor.position() as usize..], None, None)?;
        tx_outs.push(tx_out);
        cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
    }

    Ok((&data[cursor.position() as usize..], tx_ins, tx_outs))
}

/// Spend is a Sapling Spend Description as described in 7.3 of the Zcash
/// protocol specification.
#[derive(Debug, Clone)]
pub struct Spend {
    // Cv \[IGNORED\] - Size\[bytes\]: 32
    // Anchor \[IGNORED\] - Size\[bytes\]: 32
    /// A nullifier to a sapling note.
    ///
    /// Size\[bytes\]: 32
    nullifier: Vec<u8>,
    // Rk \[IGNORED\] - Size\[bytes\]: 32
    // Zkproof \[IGNORED\] - Size\[bytes\]: 192
    // SpendAuthSig \[IGNORED\] - Size\[bytes\]: 64
}

impl Spend {
    fn into_inner(self) -> Vec<u8> {
        self.nullifier
    }
}

impl ParseFromSlice for Spend {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        if txid.is_some() {
            return Err(ParseError::InvalidData(
                "txid must be None for Spend::parse_from_slice".to_string(),
            ));
        }
        let tx_version = tx_version.ok_or_else(|| {
            ParseError::InvalidData(
                "tx_version must be used for Spend::parse_from_slice".to_string(),
            )
        })?;
        let mut cursor = Cursor::new(data);

        skip_bytes(&mut cursor, 32, "Error skipping Spend::Cv")?;
        if tx_version <= 4 {
            skip_bytes(&mut cursor, 32, "Error skipping Spend::Anchor")?;
        }
        let nullifier = read_bytes(&mut cursor, 32, "Error reading Spend::nullifier")?;
        skip_bytes(&mut cursor, 32, "Error skipping Spend::Rk")?;
        if tx_version <= 4 {
            skip_bytes(&mut cursor, 192, "Error skipping Spend::Zkproof")?;
            skip_bytes(&mut cursor, 64, "Error skipping Spend::SpendAuthSig")?;
        }

        Ok((&data[cursor.position() as usize..], Spend { nullifier }))
    }
}

/// output is a Sapling Output Description as described in section 7.4 of the
/// Zcash protocol spec.
#[derive(Debug, Clone)]
pub struct Output {
    // Cv \[IGNORED\] - Size\[bytes\]: 32
    /// U-coordinate of the note commitment, derived from the note's value, recipient, and a
    /// random value.
    ///
    /// Size\[bytes\]: 32
    cmu: Vec<u8>,
    /// Ephemeral public key for Diffie-Hellman key exchange.
    ///
    /// Size\[bytes\]: 32
    ephemeral_key: Vec<u8>,
    /// Encrypted transaction details including value transferred and an optional memo.
    ///
    /// Size\[bytes\]: 580
    enc_ciphertext: Vec<u8>,
    // OutCiphertext \[IGNORED\] - Size\[bytes\]: 80
    // Zkproof \[IGNORED\] - Size\[bytes\]: 192
}

impl Output {
    fn into_parts(self) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (self.cmu, self.ephemeral_key, self.enc_ciphertext)
    }
}

impl ParseFromSlice for Output {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        if txid.is_some() {
            return Err(ParseError::InvalidData(
                "txid must be None for Output::parse_from_slice".to_string(),
            ));
        }
        let tx_version = tx_version.ok_or_else(|| {
            ParseError::InvalidData(
                "tx_version must be used for Output::parse_from_slice".to_string(),
            )
        })?;
        let mut cursor = Cursor::new(data);

        skip_bytes(&mut cursor, 32, "Error skipping Output::Cv")?;
        let cmu = read_bytes(&mut cursor, 32, "Error reading Output::cmu")?;
        let ephemeral_key = read_bytes(&mut cursor, 32, "Error reading Output::ephemeral_key")?;
        let enc_ciphertext = read_bytes(&mut cursor, 580, "Error reading Output::enc_ciphertext")?;
        skip_bytes(&mut cursor, 80, "Error skipping Output::OutCiphertext")?;
        if tx_version <= 4 {
            skip_bytes(&mut cursor, 192, "Error skipping Output::Zkproof")?;
        }

        Ok((
            &data[cursor.position() as usize..],
            Output {
                cmu,
                ephemeral_key,
                enc_ciphertext,
            },
        ))
    }
}

/// joinSplit is a JoinSplit description as described in 7.2 of the Zcash
/// protocol spec. Its exact contents differ by transaction version and network
/// upgrade level. Only version 4 is supported, no need for proofPHGR13.
///
/// NOTE: Legacy, no longer used but included for consistency.
#[derive(Debug, Clone)]
struct JoinSplit {
    //vpubOld \[IGNORED\] - Size\[bytes\]: 8
    //vpubNew \[IGNORED\] - Size\[bytes\]: 8
    //anchor \[IGNORED\] - Size\[bytes\]: 32
    //nullifiers \[IGNORED\] - Size\[bytes\]: 64/32
    //commitments \[IGNORED\] - Size\[bytes\]: 64/32
    //ephemeralKey \[IGNORED\] - Size\[bytes\]: 32
    //randomSeed \[IGNORED\] - Size\[bytes\]: 32
    //vmacs \[IGNORED\] - Size\[bytes\]: 64/32
    //proofGroth16 \[IGNORED\] - Size\[bytes\]: 192
    //encCiphertexts \[IGNORED\] - Size\[bytes\]: 1202
}

impl ParseFromSlice for JoinSplit {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        if txid.is_some() {
            return Err(ParseError::InvalidData(
                "txid must be None for JoinSplit::parse_from_slice".to_string(),
            ));
        }
        let proof_size = match tx_version {
            Some(2) | Some(3) => 296, // BCTV14 proof for v2/v3 transactions
            Some(4) => 192,           // Groth16 proof for v4 transactions
            None => 192,              // Default to Groth16 for unknown versions
            _ => {
                return Err(ParseError::InvalidData(format!(
                    "Unsupported tx_version {tx_version:?} for JoinSplit::parse_from_slice"
                )))
            }
        };
        let mut cursor = Cursor::new(data);

        skip_bytes(&mut cursor, 8, "Error skipping JoinSplit::vpubOld")?;
        skip_bytes(&mut cursor, 8, "Error skipping JoinSplit::vpubNew")?;
        skip_bytes(&mut cursor, 32, "Error skipping JoinSplit::anchor")?;
        skip_bytes(&mut cursor, 64, "Error skipping JoinSplit::nullifiers")?;
        skip_bytes(&mut cursor, 64, "Error skipping JoinSplit::commitments")?;
        skip_bytes(&mut cursor, 32, "Error skipping JoinSplit::ephemeralKey")?;
        skip_bytes(&mut cursor, 32, "Error skipping JoinSplit::randomSeed")?;
        skip_bytes(&mut cursor, 64, "Error skipping JoinSplit::vmacs")?;
        skip_bytes(
            &mut cursor,
            proof_size,
            &format!("Error skipping JoinSplit::proof (size {proof_size})"),
        )?;
        skip_bytes(
            &mut cursor,
            1202,
            "Error skipping JoinSplit::encCiphertexts",
        )?;

        Ok((&data[cursor.position() as usize..], JoinSplit {}))
    }
}

/// An Orchard action.
#[derive(Debug, Clone)]
struct Action {
    // Cv \[IGNORED\] - Size\[bytes\]: 32
    /// A nullifier to a orchard note.
    ///
    /// Size\[bytes\]: 32
    nullifier: Vec<u8>,
    // Rk \[IGNORED\] - Size\[bytes\]: 32
    /// X-coordinate of the commitment to the note.
    ///
    /// Size\[bytes\]: 32
    cmx: Vec<u8>,
    /// Ephemeral public key.
    ///
    /// Size\[bytes\]: 32
    ephemeral_key: Vec<u8>,
    /// Encrypted details of the new note, including its value and recipient's data.
    ///
    /// Size\[bytes\]: 580
    enc_ciphertext: Vec<u8>,
    // OutCiphertext \[IGNORED\] - Size\[bytes\]: 80
}

impl Action {
    fn into_parts(self) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            self.nullifier,
            self.cmx,
            self.ephemeral_key,
            self.enc_ciphertext,
        )
    }
}

impl ParseFromSlice for Action {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        if txid.is_some() {
            return Err(ParseError::InvalidData(
                "txid must be None for Action::parse_from_slice".to_string(),
            ));
        }
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for Action::parse_from_slice".to_string(),
            ));
        }
        let mut cursor = Cursor::new(data);

        skip_bytes(&mut cursor, 32, "Error skipping Action::Cv")?;
        let nullifier = read_bytes(&mut cursor, 32, "Error reading Action::nullifier")?;
        skip_bytes(&mut cursor, 32, "Error skipping Action::Rk")?;
        let cmx = read_bytes(&mut cursor, 32, "Error reading Action::cmx")?;
        let ephemeral_key = read_bytes(&mut cursor, 32, "Error reading Action::ephemeral_key")?;
        let enc_ciphertext = read_bytes(&mut cursor, 580, "Error reading Action::enc_ciphertext")?;
        skip_bytes(&mut cursor, 80, "Error skipping Action::OutCiphertext")?;

        Ok((
            &data[cursor.position() as usize..],
            Action {
                nullifier,
                cmx,
                ephemeral_key,
                enc_ciphertext,
            },
        ))
    }
}

/// Full Zcash transaction data.
#[derive(Debug, Clone)]
struct TransactionData {
    /// Indicates if the transaction is an Overwinter-enabled transaction.
    ///
    /// Size\[bytes\]: [in 4 byte header]
    f_overwintered: bool,
    /// The transaction format version.
    ///
    /// Size\[bytes\]: [in 4 byte header]
    version: u32,
    /// Version group ID, used to specify transaction type and validate its components.
    ///
    /// Size\[bytes\]: 4
    n_version_group_id: Option<u32>,
    /// Consensus branch ID, used to identify the network upgrade that the transaction is valid for.
    ///
    /// Size\[bytes\]: 4
    consensus_branch_id: u32,
    /// List of transparent inputs in a transaction.
    ///
    /// Size\[bytes\]: Vec<40+CompactSize>
    transparent_inputs: Vec<TxIn>,
    /// List of transparent outputs in a transaction.
    ///
    /// Size\[bytes\]: Vec<8+CompactSize>
    transparent_outputs: Vec<TxOut>,
    // NLockTime \[IGNORED\] - Size\[bytes\]: 4
    // NExpiryHeight \[IGNORED\] - Size\[bytes\]: 4
    // ValueBalanceSapling - Size\[bytes\]: 8
    /// Value balance for the Sapling pool (v4/v5). None if not present.
    value_balance_sapling: Option<i64>,
    /// List of shielded spends from the Sapling pool
    ///
    /// Size\[bytes\]: Vec<384>
    shielded_spends: Vec<Spend>,
    /// List of shielded outputs from the Sapling pool
    ///
    /// Size\[bytes\]: Vec<948>
    shielded_outputs: Vec<Output>,
    /// List of JoinSplit descriptions in a transaction, no longer supported.
    ///
    /// Size\[bytes\]: Vec<1602-1698>
    #[allow(dead_code)]
    join_splits: Vec<JoinSplit>,
    /// joinSplitPubKey \[IGNORED\] - Size\[bytes\]: 32
    /// joinSplitSig \[IGNORED\] - Size\[bytes\]: 64
    /// bindingSigSapling \[IGNORED\] - Size\[bytes\]: 64
    /// List of Orchard actions.
    ///
    /// Size\[bytes\]: Vec<820>
    orchard_actions: Vec<Action>,
    /// ValueBalanceOrchard - Size\[bytes\]: 8
    /// Value balance for the Orchard pool (v5 only). None if not present.
    value_balance_orchard: Option<i64>,
    /// AnchorOrchard - Size\[bytes\]: 32
    /// In non-coinbase transactions, this is the anchor (authDataRoot) of a prior block's Orchard note commitment tree.
    /// In the coinbase transaction, this commits to the final Orchard tree state for the current block — i.e., it *is* the block's authDataRoot.
    /// Present in v5 transactions only, if any Orchard actions exist in the block.
    anchor_orchard: Option<Vec<u8>>,
}

impl TransactionData {
    /// Parses a v1 transaction.
    ///
    /// A v1 transaction contains the following fields:
    ///
    /// - header: u32
    /// - tx_in_count: usize
    /// - tx_in: tx_in
    /// - tx_out_count: usize
    /// - tx_out: tx_out
    /// - lock_time: u32
    pub(crate) fn parse_v1(data: &[u8], version: u32) -> Result<(&[u8], Self), ParseError> {
        let mut cursor = Cursor::new(data);

        let (remaining_data, transparent_inputs, transparent_outputs) =
            parse_transparent(&data[cursor.position() as usize..])?;
        cursor.set_position(data.len() as u64 - remaining_data.len() as u64);

        // let lock_time = read_u32(&mut cursor, "Error reading TransactionData::lock_time")?;
        skip_bytes(&mut cursor, 4, "Error skipping TransactionData::nLockTime")?;

        Ok((
            &data[cursor.position() as usize..],
            TransactionData {
                f_overwintered: true,
                version,
                consensus_branch_id: 0,
                transparent_inputs,
                transparent_outputs,
                // lock_time: Some(lock_time),
                n_version_group_id: None,
                value_balance_sapling: None,
                shielded_spends: Vec::new(),
                shielded_outputs: Vec::new(),
                join_splits: Vec::new(),
                orchard_actions: Vec::new(),
                value_balance_orchard: None,
                anchor_orchard: None,
            },
        ))
    }

    /// Parses a v2 transaction.
    ///
    /// A v2 transaction contains the following fields:
    ///
    /// - header: u32
    /// - tx_in_count: usize
    /// - tx_in: tx_in
    /// - tx_out_count: usize
    /// - tx_out: tx_out
    /// - lock_time: u32
    /// - nJoinSplit: compactSize <- New
    /// - vJoinSplit: JSDescriptionBCTV14\[nJoinSplit\] <- New
    /// - joinSplitPubKey: byte\[32\] <- New
    /// - joinSplitSig: byte\[64\] <- New
    pub(crate) fn parse_v2(data: &[u8], version: u32) -> Result<(&[u8], Self), ParseError> {
        let mut cursor = Cursor::new(data);

        let (remaining_data, transparent_inputs, transparent_outputs) =
            parse_transparent(&data[cursor.position() as usize..])?;
        cursor.set_position(data.len() as u64 - remaining_data.len() as u64);

        skip_bytes(&mut cursor, 4, "Error skipping TransactionData::nLockTime")?;

        let join_split_count = CompactSize::read(&mut cursor)?;
        let mut join_splits = Vec::with_capacity(join_split_count as usize);
        for _ in 0..join_split_count {
            let (remaining_data, join_split) = JoinSplit::parse_from_slice(
                &data[cursor.position() as usize..],
                None,
                Some(version),
            )?;
            join_splits.push(join_split);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }

        if join_split_count > 0 {
            skip_bytes(
                &mut cursor,
                32,
                "Error skipping TransactionData::joinSplitPubKey",
            )?;
            skip_bytes(
                &mut cursor,
                64,
                "could not skip TransactionData::joinSplitSig",
            )?;
        }

        Ok((
            &data[cursor.position() as usize..],
            TransactionData {
                f_overwintered: true,
                version,
                consensus_branch_id: 0,
                transparent_inputs,
                transparent_outputs,
                join_splits,
                n_version_group_id: None,
                value_balance_sapling: None,
                shielded_spends: Vec::new(),
                shielded_outputs: Vec::new(),
                orchard_actions: Vec::new(),
                value_balance_orchard: None,
                anchor_orchard: None,
            },
        ))
    }

    /// Parses a v3 transaction.
    ///
    /// A v3 transaction contains the following fields:
    ///
    /// - header: u32
    /// - nVersionGroupId: u32 = 0x03C48270 <- New
    /// - tx_in_count: usize
    /// - tx_in: tx_in
    /// - tx_out_count: usize
    /// - tx_out: tx_out
    /// - lock_time: u32
    /// - nExpiryHeight: u32 <- New
    /// - nJoinSplit: compactSize
    /// - vJoinSplit: JSDescriptionBCTV14\[nJoinSplit\]
    /// - joinSplitPubKey: byte\[32\]
    /// - joinSplitSig: byte\[64\]
    pub(crate) fn parse_v3(
        data: &[u8],
        version: u32,
        n_version_group_id: u32,
    ) -> Result<(&[u8], Self), ParseError> {
        if n_version_group_id != 0x03C48270 {
            return Err(ParseError::InvalidData(
                "n_version_group_id must be 0x03C48270".to_string(),
            ));
        }
        let mut cursor = Cursor::new(data);

        let (remaining_data, transparent_inputs, transparent_outputs) =
            parse_transparent(&data[cursor.position() as usize..])?;
        cursor.set_position(data.len() as u64 - remaining_data.len() as u64);

        skip_bytes(&mut cursor, 4, "Error skipping TransactionData::nLockTime")?;
        skip_bytes(
            &mut cursor,
            4,
            "Error skipping TransactionData::nExpiryHeight",
        )?;

        let join_split_count = CompactSize::read(&mut cursor)?;
        let mut join_splits = Vec::with_capacity(join_split_count as usize);
        for _ in 0..join_split_count {
            let (remaining_data, join_split) = JoinSplit::parse_from_slice(
                &data[cursor.position() as usize..],
                None,
                Some(version),
            )?;
            join_splits.push(join_split);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }

        if join_split_count > 0 {
            skip_bytes(
                &mut cursor,
                32,
                "Error skipping TransactionData::joinSplitPubKey",
            )?;
            skip_bytes(
                &mut cursor,
                64,
                "could not skip TransactionData::joinSplitSig",
            )?;
        }
        Ok((
            &data[cursor.position() as usize..],
            TransactionData {
                f_overwintered: true,
                version,
                consensus_branch_id: 0,
                transparent_inputs,
                transparent_outputs,
                join_splits,
                n_version_group_id: None,
                value_balance_sapling: None,
                shielded_spends: Vec::new(),
                shielded_outputs: Vec::new(),
                orchard_actions: Vec::new(),
                value_balance_orchard: None,
                anchor_orchard: None,
            },
        ))
    }

    fn parse_v4(
        data: &[u8],
        version: u32,
        n_version_group_id: u32,
    ) -> Result<(&[u8], Self), ParseError> {
        if n_version_group_id != 0x892F2085 {
            return Err(ParseError::InvalidData(format!(
                "version group ID {n_version_group_id:x} must be 0x892F2085 for v4 transactions"
            )));
        }
        let mut cursor = Cursor::new(data);

        let (remaining_data, transparent_inputs, transparent_outputs) =
            parse_transparent(&data[cursor.position() as usize..])?;
        cursor.set_position(data.len() as u64 - remaining_data.len() as u64);

        skip_bytes(&mut cursor, 4, "Error skipping TransactionData::nLockTime")?;
        skip_bytes(
            &mut cursor,
            4,
            "Error skipping TransactionData::nExpiryHeight",
        )?;
        let value_balance_sapling = Some(read_i64(
            &mut cursor,
            "Error reading TransactionData::valueBalanceSapling",
        )?);

        let spend_count = CompactSize::read(&mut cursor)?;
        let mut shielded_spends = Vec::with_capacity(spend_count as usize);
        for _ in 0..spend_count {
            let (remaining_data, spend) =
                Spend::parse_from_slice(&data[cursor.position() as usize..], None, Some(4))?;
            shielded_spends.push(spend);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }
        let output_count = CompactSize::read(&mut cursor)?;
        let mut shielded_outputs = Vec::with_capacity(output_count as usize);
        for _ in 0..output_count {
            let (remaining_data, output) =
                Output::parse_from_slice(&data[cursor.position() as usize..], None, Some(4))?;
            shielded_outputs.push(output);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }
        let join_split_count = CompactSize::read(&mut cursor)?;
        let mut join_splits = Vec::with_capacity(join_split_count as usize);
        for _ in 0..join_split_count {
            let (remaining_data, join_split) = JoinSplit::parse_from_slice(
                &data[cursor.position() as usize..],
                None,
                Some(version),
            )?;
            join_splits.push(join_split);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }

        if join_split_count > 0 {
            skip_bytes(
                &mut cursor,
                32,
                "Error skipping TransactionData::joinSplitPubKey",
            )?;
            skip_bytes(
                &mut cursor,
                64,
                "could not skip TransactionData::joinSplitSig",
            )?;
        }
        if spend_count + output_count > 0 {
            skip_bytes(
                &mut cursor,
                64,
                "Error skipping TransactionData::bindingSigSapling",
            )?;
        }

        Ok((
            &data[cursor.position() as usize..],
            TransactionData {
                f_overwintered: true,
                version,
                n_version_group_id: Some(n_version_group_id),
                consensus_branch_id: 0,
                transparent_inputs,
                transparent_outputs,
                value_balance_sapling,
                shielded_spends,
                shielded_outputs,
                join_splits,
                orchard_actions: Vec::new(),
                value_balance_orchard: None,
                anchor_orchard: None,
            },
        ))
    }

    fn parse_v5(
        data: &[u8],
        version: u32,
        n_version_group_id: u32,
    ) -> Result<(&[u8], Self), ParseError> {
        if n_version_group_id != 0x26A7270A {
            return Err(ParseError::InvalidData(format!(
                "version group ID {n_version_group_id:x} must be 0x892F2085 for v5 transactions"
            )));
        }
        let mut cursor = Cursor::new(data);

        let consensus_branch_id = read_u32(
            &mut cursor,
            "Error reading TransactionData::ConsensusBranchId",
        )?;

        skip_bytes(&mut cursor, 4, "Error skipping TransactionData::nLockTime")?;
        skip_bytes(
            &mut cursor,
            4,
            "Error skipping TransactionData::nExpiryHeight",
        )?;

        let (remaining_data, transparent_inputs, transparent_outputs) =
            parse_transparent(&data[cursor.position() as usize..])?;
        cursor.set_position(data.len() as u64 - remaining_data.len() as u64);

        let spend_count = CompactSize::read(&mut cursor)?;
        if spend_count >= (1 << 16) {
            return Err(ParseError::InvalidData(format!(
                "spendCount ({spend_count}) must be less than 2^16"
            )));
        }
        let mut shielded_spends = Vec::with_capacity(spend_count as usize);
        for _ in 0..spend_count {
            let (remaining_data, spend) =
                Spend::parse_from_slice(&data[cursor.position() as usize..], None, Some(5))?;
            shielded_spends.push(spend);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }
        let output_count = CompactSize::read(&mut cursor)?;
        if output_count >= (1 << 16) {
            return Err(ParseError::InvalidData(format!(
                "outputCount ({output_count}) must be less than 2^16"
            )));
        }
        let mut shielded_outputs = Vec::with_capacity(output_count as usize);
        for _ in 0..output_count {
            let (remaining_data, output) =
                Output::parse_from_slice(&data[cursor.position() as usize..], None, Some(5))?;
            shielded_outputs.push(output);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }

        let value_balance_sapling = if spend_count + output_count > 0 {
            Some(read_i64(
                &mut cursor,
                "Error reading TransactionData::valueBalanceSapling",
            )?)
        } else {
            None
        };
        if spend_count > 0 {
            skip_bytes(
                &mut cursor,
                32,
                "Error skipping TransactionData::anchorSapling",
            )?;
            skip_bytes(
                &mut cursor,
                (192 * spend_count) as usize,
                "Error skipping TransactionData::vSpendProofsSapling",
            )?;
            skip_bytes(
                &mut cursor,
                (64 * spend_count) as usize,
                "Error skipping TransactionData::vSpendAuthSigsSapling",
            )?;
        }
        if output_count > 0 {
            skip_bytes(
                &mut cursor,
                (192 * output_count) as usize,
                "Error skipping TransactionData::vOutputProofsSapling",
            )?;
        }
        if spend_count + output_count > 0 {
            skip_bytes(
                &mut cursor,
                64,
                "Error skipping TransactionData::bindingSigSapling",
            )?;
        }

        let actions_count = CompactSize::read(&mut cursor)?;
        if actions_count >= (1 << 16) {
            return Err(ParseError::InvalidData(format!(
                "actionsCount ({actions_count}) must be less than 2^16"
            )));
        }
        let mut orchard_actions = Vec::with_capacity(actions_count as usize);
        for _ in 0..actions_count {
            let (remaining_data, action) =
                Action::parse_from_slice(&data[cursor.position() as usize..], None, None)?;
            orchard_actions.push(action);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }

        let mut value_balance_orchard = None;
        let mut anchor_orchard = None;
        if actions_count > 0 {
            skip_bytes(
                &mut cursor,
                1,
                "Error skipping TransactionData::flagsOrchard",
            )?;
            value_balance_orchard = Some(read_i64(
                &mut cursor,
                "Error reading TransactionData::valueBalanceOrchard",
            )?);
            anchor_orchard = Some(read_bytes(
                &mut cursor,
                32,
                "Error reading TransactionData::anchorOrchard",
            )?);
            let proofs_count = CompactSize::read(&mut cursor)?;
            skip_bytes(
                &mut cursor,
                proofs_count as usize,
                "Error skipping TransactionData::proofsOrchard",
            )?;
            skip_bytes(
                &mut cursor,
                (64 * actions_count) as usize,
                "Error skipping TransactionData::vSpendAuthSigsOrchard",
            )?;
            skip_bytes(
                &mut cursor,
                64,
                "Error skipping TransactionData::bindingSigOrchard",
            )?;
        }

        Ok((
            &data[cursor.position() as usize..],
            TransactionData {
                f_overwintered: true,
                version,
                n_version_group_id: Some(n_version_group_id),
                consensus_branch_id,
                transparent_inputs,
                transparent_outputs,
                value_balance_sapling,
                shielded_spends,
                shielded_outputs,
                join_splits: Vec::new(),
                orchard_actions,
                value_balance_orchard,
                anchor_orchard,
            },
        ))
    }
}

/// Zingo-Indexer struct for a full zcash transaction.
#[derive(Debug, Clone)]
pub struct FullTransaction {
    /// Full transaction data.
    raw_transaction: TransactionData,

    /// Raw transaction bytes.
    raw_bytes: Vec<u8>,

    /// Transaction Id, fetched using get_block JsonRPC with verbose = 1.
    tx_id: Vec<u8>,
}

impl ParseFromSlice for FullTransaction {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        let txid = txid.ok_or_else(|| {
            ParseError::InvalidData(
                "txid must be used for FullTransaction::parse_from_slice".to_string(),
            )
        })?;
        // TODO: 🤯
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for FullTransaction::parse_from_slice".to_string(),
            ));
        }
        let mut cursor = Cursor::new(data);

        let header = read_u32(&mut cursor, "Error reading FullTransaction::header")?;
        let f_overwintered = (header >> 31) == 1;

        let version = header & 0x7FFFFFFF;

        match version {
            1 | 2 => {
                if f_overwintered {
                    return Err(ParseError::InvalidData(
                        "fOverwintered must be unset for tx versions 1 and 2".to_string(),
                    ));
                }
            }
            3..=5 => {
                if !f_overwintered {
                    return Err(ParseError::InvalidData(
                        "fOverwintered must be set for tx versions 3 and above".to_string(),
                    ));
                }
            }
            _ => {
                return Err(ParseError::InvalidData(format!(
                    "Unsupported tx version {version}"
                )))
            }
        }

        let n_version_group_id: Option<u32> = match version {
            3..=5 => Some(read_u32(
                &mut cursor,
                "Error reading FullTransaction::n_version_group_id",
            )?),
            _ => None,
        };

        let (remaining_data, transaction_data) = match version {
            1 => TransactionData::parse_v1(&data[cursor.position() as usize..], version)?,
            2 => TransactionData::parse_v2(&data[cursor.position() as usize..], version)?,
            3 => TransactionData::parse_v3(
                &data[cursor.position() as usize..],
                version,
                n_version_group_id.unwrap(), // This won't fail, because of the above match
            )?,
            4 => TransactionData::parse_v4(
                &data[cursor.position() as usize..],
                version,
                n_version_group_id.unwrap(), // This won't fail, because of the above match
            )?,
            5 => TransactionData::parse_v5(
                &data[cursor.position() as usize..],
                version,
                n_version_group_id.unwrap(), // This won't fail, because of the above match
            )?,

            _ => {
                return Err(ParseError::InvalidData(format!(
                    "Unsupported tx version {version}"
                )))
            }
        };

        let full_transaction = FullTransaction {
            raw_transaction: transaction_data,
            raw_bytes: data[..(data.len() - remaining_data.len())].to_vec(),
            tx_id: txid[0].clone(),
        };

        Ok((remaining_data, full_transaction))
    }
}

impl FullTransaction {
    /// Returns overwintered bool
    pub fn f_overwintered(&self) -> bool {
        self.raw_transaction.f_overwintered
    }

    /// Returns the transaction version.
    pub fn version(&self) -> u32 {
        self.raw_transaction.version
    }

    /// Returns the transaction version group id.
    pub fn n_version_group_id(&self) -> Option<u32> {
        self.raw_transaction.n_version_group_id
    }

    /// returns the consensus branch id of the transaction.
    pub fn consensus_branch_id(&self) -> u32 {
        self.raw_transaction.consensus_branch_id
    }

    /// Returns a vec of transparent inputs: (prev_txid, prev_index, script_sig).
    pub fn transparent_inputs(&self) -> Vec<(Vec<u8>, u32, Vec<u8>)> {
        self.raw_transaction
            .transparent_inputs
            .iter()
            .map(|input| input.clone().into_inner())
            .collect()
    }

    /// Returns a vec of transparent outputs: (value, script_hash).
    pub fn transparent_outputs(&self) -> Vec<(u64, Vec<u8>)> {
        self.raw_transaction
            .transparent_outputs
            .iter()
            .map(|output| output.clone().into_inner())
            .collect()
    }

    /// Returns sapling and orchard value balances for the transaction.
    ///
    /// Returned as (Option\<valueBalanceSapling\>, Option\<valueBalanceOrchard\>).
    pub fn value_balances(&self) -> (Option<i64>, Option<i64>) {
        (
            self.raw_transaction.value_balance_sapling,
            self.raw_transaction.value_balance_orchard,
        )
    }

    /// Returns a vec of sapling nullifiers for the transaction.
    pub fn shielded_spends(&self) -> Vec<Vec<u8>> {
        self.raw_transaction
            .shielded_spends
            .iter()
            .map(|input| input.clone().into_inner())
            .collect()
    }

    /// Returns a vec of sapling outputs (cmu, ephemeral_key, enc_ciphertext) for the transaction.
    pub fn shielded_outputs(&self) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        self.raw_transaction
            .shielded_outputs
            .iter()
            .map(|input| input.clone().into_parts())
            .collect()
    }

    /// Returns None as joinsplits are not supported in Zaino.
    pub fn join_splits(&self) -> Option<()> {
        None
    }

    /// Returns a vec of orchard actions (nullifier, cmx, ephemeral_key, enc_ciphertext) for the transaction.
    #[allow(clippy::complexity)]
    pub fn orchard_actions(&self) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
        self.raw_transaction
            .orchard_actions
            .iter()
            .map(|input| input.clone().into_parts())
            .collect()
    }

    /// Returns the orchard anchor of the transaction.
    ///
    /// If this is the Coinbase transaction then this returns the AuthDataRoot of the block.
    pub fn anchor_orchard(&self) -> Option<Vec<u8>> {
        self.raw_transaction.anchor_orchard.clone()
    }

    /// Returns the transaction as raw bytes.
    pub fn raw_bytes(&self) -> Vec<u8> {
        self.raw_bytes.clone()
    }

    /// returns the TxId of the transaction.
    pub fn tx_id(&self) -> Vec<u8> {
        self.tx_id.clone()
    }

    /// Converts a zcash full transaction into a compact transaction.
    pub fn to_compact(self, index: u64) -> Result<CompactTx, ParseError> {
        let hash = self.tx_id;

        // NOTE: LightWalletD currently does not return a fee and is not currently priority here. Please open an Issue or PR at the Zingo-Indexer github (https://github.com/zingolabs/zingo-indexer) if you require this functionality.
        let fee = 0;

        let spends = self
            .raw_transaction
            .shielded_spends
            .iter()
            .map(|spend| CompactSaplingSpend {
                nf: spend.nullifier.clone(),
            })
            .collect();

        let outputs = self
            .raw_transaction
            .shielded_outputs
            .iter()
            .map(|output| CompactSaplingOutput {
                cmu: output.cmu.clone(),
                ephemeral_key: output.ephemeral_key.clone(),
                ciphertext: output.enc_ciphertext[..52].to_vec(),
            })
            .collect();

        let actions = self
            .raw_transaction
            .orchard_actions
            .iter()
            .map(|action| CompactOrchardAction {
                nullifier: action.nullifier.clone(),
                cmx: action.cmx.clone(),
                ephemeral_key: action.ephemeral_key.clone(),
                ciphertext: action.enc_ciphertext[..52].to_vec(),
            })
            .collect();

        Ok(CompactTx {
            index,
            hash,
            fee,
            spends,
            outputs,
            actions,
        })
    }

    /// Returns true if the transaction contains either sapling spends or outputs.
    pub(crate) fn has_shielded_elements(&self) -> bool {
        !self.raw_transaction.shielded_spends.is_empty()
            || !self.raw_transaction.shielded_outputs.is_empty()
            || !self.raw_transaction.orchard_actions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_testvectors::transactions::get_test_vectors;

    /// Test parsing v1 transactions using test vectors.
    /// Validates that FullTransaction::parse_from_slice correctly handles v1 transaction format.
    #[test]
    fn test_v1_transaction_parsing_with_test_vectors() {
        let test_vectors = get_test_vectors();
        let v1_vectors: Vec<_> = test_vectors.iter().filter(|tv| tv.version == 1).collect();

        assert!(!v1_vectors.is_empty(), "No v1 test vectors found");

        for (i, vector) in v1_vectors.iter().enumerate() {
            let result = FullTransaction::parse_from_slice(
                &vector.tx,
                Some(vec![vector.txid.to_vec()]),
                None,
            );

            assert!(
                result.is_ok(),
                "Failed to parse v1 test vector #{}: {:?}. Description: {}",
                i,
                result.err(),
                vector.description
            );

            let (remaining, parsed_tx) = result.unwrap();
            assert!(
                remaining.is_empty(),
                "Should consume all data for v1 transaction #{i}"
            );

            // Verify version matches
            assert_eq!(
                parsed_tx.raw_transaction.version, 1,
                "Version mismatch for v1 transaction #{i}"
            );

            // Verify transaction properties match test vector expectations
            assert_eq!(
                parsed_tx.raw_transaction.transparent_inputs.len(),
                vector.transparent_inputs,
                "Transparent inputs mismatch for v1 transaction #{i}"
            );

            assert_eq!(
                parsed_tx.raw_transaction.transparent_outputs.len(),
                vector.transparent_outputs,
                "Transparent outputs mismatch for v1 transaction #{i}"
            );
        }
    }

    /// Test parsing v2 transactions using test vectors.
    /// Validates that FullTransaction::parse_from_slice correctly handles v2 transaction format.
    #[test]
    fn test_v2_transaction_parsing_with_test_vectors() {
        let test_vectors = get_test_vectors();
        let v2_vectors: Vec<_> = test_vectors.iter().filter(|tv| tv.version == 2).collect();

        assert!(!v2_vectors.is_empty(), "No v2 test vectors found");

        for (i, vector) in v2_vectors.iter().enumerate() {
            let result = FullTransaction::parse_from_slice(
                &vector.tx,
                Some(vec![vector.txid.to_vec()]),
                None,
            );

            assert!(
                result.is_ok(),
                "Failed to parse v2 test vector #{}: {:?}. Description: {}",
                i,
                result.err(),
                vector.description
            );

            let (remaining, parsed_tx) = result.unwrap();
            assert!(
                remaining.is_empty(),
                "Should consume all data for v2 transaction #{}: {} bytes remaining, total length: {}",
                i, remaining.len(), vector.tx.len()
            );

            // Verify version matches
            assert_eq!(
                parsed_tx.raw_transaction.version, 2,
                "Version mismatch for v2 transaction #{i}"
            );

            // Verify transaction properties match test vector expectations
            assert_eq!(
                parsed_tx.raw_transaction.transparent_inputs.len(),
                vector.transparent_inputs,
                "Transparent inputs mismatch for v2 transaction #{i}"
            );

            assert_eq!(
                parsed_tx.raw_transaction.transparent_outputs.len(),
                vector.transparent_outputs,
                "Transparent outputs mismatch for v2 transaction #{i}"
            );
        }
    }

    /// Test parsing v3 transactions using test vectors.
    /// Validates that FullTransaction::parse_from_slice correctly handles v3 transaction format.
    #[test]
    fn test_v3_transaction_parsing_with_test_vectors() {
        let test_vectors = get_test_vectors();
        let v3_vectors: Vec<_> = test_vectors.iter().filter(|tv| tv.version == 3).collect();

        assert!(!v3_vectors.is_empty(), "No v3 test vectors found");

        for (i, vector) in v3_vectors.iter().enumerate() {
            let result = FullTransaction::parse_from_slice(
                &vector.tx,
                Some(vec![vector.txid.to_vec()]),
                None,
            );

            assert!(
                result.is_ok(),
                "Failed to parse v3 test vector #{}: {:?}. Description: {}",
                i,
                result.err(),
                vector.description
            );

            let (remaining, parsed_tx) = result.unwrap();
            assert!(
                remaining.is_empty(),
                "Should consume all data for v3 transaction #{}: {} bytes remaining, total length: {}",
                i, remaining.len(), vector.tx.len()
            );

            // Verify version matches
            assert_eq!(
                parsed_tx.raw_transaction.version, 3,
                "Version mismatch for v3 transaction #{i}"
            );

            // Verify transaction properties match test vector expectations
            assert_eq!(
                parsed_tx.raw_transaction.transparent_inputs.len(),
                vector.transparent_inputs,
                "Transparent inputs mismatch for v3 transaction #{i}"
            );

            assert_eq!(
                parsed_tx.raw_transaction.transparent_outputs.len(),
                vector.transparent_outputs,
                "Transparent outputs mismatch for v3 transaction #{i}"
            );
        }
    }

    /// Test parsing v4 transactions using test vectors.
    /// Validates that FullTransaction::parse_from_slice correctly handles v4 transaction format.
    /// This also serves as a regression test for current v4 functionality.
    #[test]
    fn test_v4_transaction_parsing_with_test_vectors() {
        let test_vectors = get_test_vectors();
        let v4_vectors: Vec<_> = test_vectors.iter().filter(|tv| tv.version == 4).collect();

        assert!(!v4_vectors.is_empty(), "No v4 test vectors found");

        for (i, vector) in v4_vectors.iter().enumerate() {
            let result = FullTransaction::parse_from_slice(
                &vector.tx,
                Some(vec![vector.txid.to_vec()]),
                None,
            );

            assert!(
                result.is_ok(),
                "Failed to parse v4 test vector #{}: {:?}. Description: {}",
                i,
                result.err(),
                vector.description
            );

            let (remaining, parsed_tx) = result.unwrap();
            assert!(
                remaining.is_empty(),
                "Should consume all data for v4 transaction #{i}"
            );

            // Verify version matches
            assert_eq!(
                parsed_tx.raw_transaction.version, 4,
                "Version mismatch for v4 transaction #{i}"
            );

            // Verify transaction properties match test vector expectations
            assert_eq!(
                parsed_tx.raw_transaction.transparent_inputs.len(),
                vector.transparent_inputs,
                "Transparent inputs mismatch for v4 transaction #{i}"
            );

            assert_eq!(
                parsed_tx.raw_transaction.transparent_outputs.len(),
                vector.transparent_outputs,
                "Transparent outputs mismatch for v4 transaction #{i}"
            );
        }
    }
}
