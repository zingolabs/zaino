//! Transaction fetching and deserialization functionality.

use crate::blockcache::utils::{
    read_bytes, read_u32, read_u64, skip_bytes, ParseError, ParseFromSlice,
};
use std::io::Cursor;
use zcash_client_backend::proto::compact_formats::{
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTx,
};
use zcash_encoding::CompactSize;

/// Txin format as described in https://en.bitcoin.it/wiki/Transaction
#[derive(Debug)]
pub struct TxIn {
    // PrevTxHash [IGNORED] - Size[bytes]: 32
    // PrevTxOutIndex [IGNORED] - Size[bytes]: 4
    /// CompactSize-prefixed, could be a pubkey or a script
    ///
    /// Size[bytes]: CompactSize
    pub script_sig: Vec<u8>,
    // SequenceNumber [IGNORED] - Size[bytes]: 4
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

        skip_bytes(&mut cursor, 32, "Error skipping TxIn::PrevTxHash")?;
        skip_bytes(&mut cursor, 4, "Error skipping TxIn::PrevTxOutIndex")?;
        let script_sig = {
            let compact_length = CompactSize::read(&mut cursor)?;
            read_bytes(
                &mut cursor,
                compact_length as usize,
                "Error reading TxIn::ScriptSig",
            )?
        };
        skip_bytes(&mut cursor, 4, "Error skipping TxIn::SequenceNumber")?;

        Ok((&data[cursor.position() as usize..], TxIn { script_sig }))
    }
}

/// Txout format as described in https://en.bitcoin.it/wiki/Transaction
#[derive(Debug)]
pub struct TxOut {
    /// Non-negative int giving the number of zatoshis to be transferred
    ///
    /// Size[bytes]: 8
    pub value: u64,
    // Script [IGNORED] - Size[bytes]: CompactSize
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
        let compact_length = CompactSize::read(&mut cursor)?;
        skip_bytes(
            &mut cursor,
            compact_length as usize,
            "Error skipping TxOut::Script",
        )?;

        Ok((&data[cursor.position() as usize..], TxOut { value }))
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

/// spend is a Sapling Spend Description as described in 7.3 of the Zcash
/// protocol specification.
#[derive(Debug)]
pub struct Spend {
    // Cv [IGNORED] - Size[bytes]: 32
    // Anchor [IGNORED] - Size[bytes]: 32
    /// A nullifier to a sapling note.
    ///
    /// Size[bytes]: 32
    pub nullifier: Vec<u8>,
    // Rk [IGNORED] - Size[bytes]: 32
    // Zkproof [IGNORED] - Size[bytes]: 192
    // SpendAuthSig [IGNORED] - Size[bytes]: 64
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
#[derive(Debug)]
pub struct Output {
    // Cv [IGNORED] - Size[bytes]: 32
    /// U-coordinate of the note commitment, derived from the note's value, recipient, and a
    /// random value.
    ///
    /// Size[bytes]: 32
    pub cmu: Vec<u8>,
    /// Ephemeral public key for Diffie-Hellman key exchange.
    ///
    /// Size[bytes]: 32
    pub ephemeral_key: Vec<u8>,
    /// Encrypted transaction details including value transferred and an optional memo.
    ///
    /// Size[bytes]: 580
    pub enc_ciphertext: Vec<u8>,
    // OutCiphertext [IGNORED] - Size[bytes]: 80
    // Zkproof [IGNORED] - Size[bytes]: 192
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
#[derive(Debug)]
pub struct JoinSplit {
    //vpubOld [IGNORED] - Size[bytes]: 8
    //vpubNew [IGNORED] - Size[bytes]: 8
    //anchor [IGNORED] - Size[bytes]: 32
    //nullifiers [IGNORED] - Size[bytes]: 64/32
    //commitments [IGNORED] - Size[bytes]: 64/32
    //ephemeralKey [IGNORED] - Size[bytes]: 32
    //randomSeed [IGNORED] - Size[bytes]: 32
    //vmacs [IGNORED] - Size[bytes]: 64/32
    //proofGroth16 [IGNORED] - Size[bytes]: 192
    //encCiphertexts [IGNORED] - Size[bytes]: 1202
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
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for JoinSplit::parse_from_slice".to_string(),
            ));
        }
        let mut cursor = Cursor::new(data);

        skip_bytes(&mut cursor, 8, "Error skipping JoinSplit::vpubOld")?;
        skip_bytes(&mut cursor, 8, "Error skipping JoinSplit::vpubNew")?;
        skip_bytes(&mut cursor, 32, "Error skipping JoinSplit::anchor")?;
        skip_bytes(&mut cursor, 64, "Error skipping JoinSplit::nullifiers")?;
        skip_bytes(&mut cursor, 64, "Error skipping JoinSplit::commitments")?;
        skip_bytes(&mut cursor, 32, "Error skipping JoinSplit::ephemeralKey")?;
        skip_bytes(&mut cursor, 32, "Error skipping JoinSplit::randomSeed")?;
        skip_bytes(&mut cursor, 64, "Error skipping JoinSplit::vmacs")?;
        skip_bytes(&mut cursor, 192, "Error skipping JoinSplit::proofGroth16")?;
        skip_bytes(
            &mut cursor,
            1202,
            "Error skipping JoinSplit::encCiphertexts",
        )?;

        Ok((&data[cursor.position() as usize..], JoinSplit {}))
    }
}

/// An Orchard action.
#[derive(Debug)]
pub struct Action {
    // Cv [IGNORED] - Size[bytes]: 32
    /// A nullifier to a orchard note.
    ///
    /// Size[bytes]: 32
    pub nullifier: Vec<u8>,
    // Rk [IGNORED] - Size[bytes]: 32
    /// X-coordinate of the commitment to the note.
    ///
    /// Size[bytes]: 32
    pub cmx: Vec<u8>,
    /// Ephemeral public key.
    ///
    /// Size[bytes]: 32
    pub ephemeral_key: Vec<u8>,
    /// Encrypted details of the new note, including its value and recipient's data.
    ///
    /// Size[bytes]: 580
    pub enc_ciphertext: Vec<u8>,
    // OutCiphertext [IGNORED] - Size[bytes]: 80
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

/// Full Zcash Transactrion data.
#[derive(Debug)]
pub struct TransactionData {
    /// Indicates if the transaction is an Overwinter-enabled transaction.
    ///
    /// Size[bytes]: [in 4 byte header]
    pub f_overwintered: bool,
    /// The transaction format version.
    ///
    /// Size[bytes]: [in 4 byte header]
    pub version: u32,
    /// Version group ID, used to specify transaction type and validate its components.
    ///
    /// Size[bytes]: 4
    pub n_version_group_id: u32,
    /// Consensus branch ID, used to identify the network upgrade that the transaction is valid for.
    ///
    /// Size[bytes]: 4
    pub consensus_branch_id: u32,
    /// List of transparent inputs in a transaction.
    ///
    /// Size[bytes]: Vec<40+CompactSize>
    pub transparent_inputs: Vec<TxIn>,
    /// List of transparent outputs in a transaction.
    ///
    /// Size[bytes]: Vec<8+CompactSize>
    pub transparent_outputs: Vec<TxOut>,
    // NLockTime [IGNORED] - Size[bytes]: 4
    // NExpiryHeight [IGNORED] - Size[bytes]: 4
    // ValueBalanceSapling [IGNORED] - Size[bytes]: 8
    /// List of shielded spends from the Sapling pool
    ///
    /// Size[bytes]: Vec<384>
    pub shielded_spends: Vec<Spend>,
    /// List of shielded outputs from the Sapling pool
    ///
    /// Size[bytes]: Vec<948>
    pub shielded_outputs: Vec<Output>,
    /// List of JoinSplit descriptions in a transaction, no longer supported.
    ///
    /// Size[bytes]: Vec<1602-1698>
    pub join_splits: Vec<JoinSplit>,
    //joinSplitPubKey [IGNORED] - Size[bytes]: 32
    //joinSplitSig [IGNORED] - Size[bytes]: 64
    //bindingSigSapling [IGNORED] - Size[bytes]: 64
    ///List of Orchard actions.
    ///
    /// Size[bytes]: Vec<820>
    pub orchard_actions: Vec<Action>,
}

impl TransactionData {
    fn parse_v4(
        data: &[u8],
        version: u32,
        n_version_group_id: u32,
    ) -> Result<(&[u8], Self), ParseError> {
        if n_version_group_id != 0x892F2085 {
            return Err(ParseError::InvalidData(format!(
                "version group ID {:x} must be 0x892F2085 for v4 transactions",
                n_version_group_id
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
        skip_bytes(
            &mut cursor,
            8,
            "Error skipping TransactionData::valueBalance",
        )?;

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
            let (remaining_data, join_split) =
                JoinSplit::parse_from_slice(&data[cursor.position() as usize..], None, None)?;
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
                n_version_group_id,
                consensus_branch_id: 0,
                transparent_inputs,
                transparent_outputs,
                shielded_spends,
                shielded_outputs,
                join_splits,
                orchard_actions: Vec::new(),
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
                "version group ID {:x} must be 0x892F2085 for v5 transactions",
                n_version_group_id
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
                "spendCount ({}) must be less than 2^16",
                spend_count
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
                "outputCount ({}) must be less than 2^16",
                output_count
            )));
        }
        let mut shielded_outputs = Vec::with_capacity(output_count as usize);
        for _ in 0..output_count {
            let (remaining_data, output) =
                Output::parse_from_slice(&data[cursor.position() as usize..], None, Some(5))?;
            shielded_outputs.push(output);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }

        if spend_count + output_count > 0 {
            skip_bytes(
                &mut cursor,
                8,
                "Error skipping TransactionData::valueBalance",
            )?;
        }
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
                "actionsCount ({}) must be less than 2^16",
                actions_count
            )));
        }
        let mut orchard_actions = Vec::with_capacity(actions_count as usize);
        for _ in 0..actions_count {
            let (remaining_data, action) =
                Action::parse_from_slice(&data[cursor.position() as usize..], None, None)?;
            orchard_actions.push(action);
            cursor.set_position(data.len() as u64 - remaining_data.len() as u64);
        }

        if actions_count > 0 {
            skip_bytes(
                &mut cursor,
                1,
                "Error skipping TransactionData::flagsOrchard",
            )?;
            skip_bytes(
                &mut cursor,
                8,
                "Error skipping TransactionData::valueBalanceOrchard",
            )?;
            skip_bytes(
                &mut cursor,
                32,
                "Error skipping TransactionData::anchorOrchard",
            )?;

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
                n_version_group_id,
                consensus_branch_id,
                transparent_inputs,
                transparent_outputs,
                shielded_spends,
                shielded_outputs,
                join_splits: Vec::new(),
                orchard_actions,
            },
        ))
    }
}

/// Zingo-Proxy struct for a full zcash transaction.
#[derive(Debug)]
pub struct FullTransaction {
    /// Full transaction data.
    pub raw_transaction: TransactionData,

    /// Raw transaction bytes.
    pub raw_bytes: Vec<u8>,

    /// Transaction Id, fetched using get_block JsonRPC with verbose = 1.
    pub tx_id: Vec<u8>,
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
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for FullTransaction::parse_from_slice".to_string(),
            ));
        }
        let mut cursor = Cursor::new(data);

        let header = read_u32(&mut cursor, "Error reading FullTransaction::header")?;
        let f_overwintered = (header >> 31) == 1;
        if !f_overwintered {
            return Err(ParseError::InvalidData(
                "fOverwinter flag must be set".to_string(),
            ));
        }
        let version = header & 0x7FFFFFFF;
        if version < 4 {
            return Err(ParseError::InvalidData(format!(
                "version number {} must be greater or equal to 4",
                version
            )));
        }
        let n_version_group_id = read_u32(
            &mut cursor,
            "Error reading FullTransaction::n_version_group_id",
        )?;

        let (remaining_data, transaction_data) = if version <= 4 {
            TransactionData::parse_v4(
                &data[cursor.position() as usize..],
                version,
                n_version_group_id,
            )?
        } else {
            TransactionData::parse_v5(
                &data[cursor.position() as usize..],
                version,
                n_version_group_id,
            )?
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
    /// Converts a zcash full transaction into a compact transaction.
    pub fn to_compact(self, index: u64) -> Result<CompactTx, ParseError> {
        let hash = self.tx_id;

        // NOTE: LightWalletD currently does not return a fee and is not currently priority here. Please open an Issue or PR at the Zingo-Proxy github (https://github.com/zingolabs/zingo-proxy) if you require this functionality.
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
    pub fn has_shielded_elements(&self) -> bool {
        !self.raw_transaction.shielded_spends.is_empty()
            || !self.raw_transaction.shielded_outputs.is_empty()
            || !self.raw_transaction.orchard_actions.is_empty()
    }
}
