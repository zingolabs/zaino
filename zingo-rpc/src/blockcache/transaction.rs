//! Transaction fetching and deserialization functionality.

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

/// Txout format as described in https://en.bitcoin.it/wiki/Transaction
#[derive(Debug)]
pub struct TxOut {
    /// Non-negative int giving the number of zatoshis to be transferred
    ///
    /// Size[bytes]: 8
    pub value: u64,
    // Script [IGNORED] - Size[bytes]: CompactSize
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

// impl parse_from_slice for TxIn(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_from_slice for TxOut(&[u8]) -> Result<(Self, &[u8]), ParseError>

// imple parse_transparent(&[u8]) -> Result<(????, &[u8]), ParseError>

// impl parse_from_slice for Spend(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_from_slice for Output(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_from_slice for JoinSplit(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_from_slice for Action(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl parse_v4(&[u8]) -> Result<(????, &[u8]), ParseError>

// impl parse_v5(&[u8]) -> Result<(????, &[u8]), ParseError>

// impl parse_from_slice for transaction(&[u8]) -> Result<(Self, &[u8]), ParseError>

// impl to_compact(Self) -> Result<compact_transaction, Error>

// impl parse_to_compact(&[u8]) -> Result<(compact_transaction, &[u8]), Error>
