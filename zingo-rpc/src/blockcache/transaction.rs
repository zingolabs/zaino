//! Transaction fetching and deserialization functionality.

/// Transparent input.
#[derive(Debug)]
pub struct TxIn {
    pub script_sig: Vec<u8>,
}

/// Transparent output.
#[derive(Debug)]
pub struct TxOut {
    pub value: u64,
}

/// Sapling input.
#[derive(Debug)]
pub struct Spend {
    pub nullifier: Vec<u8>,
}

/// Sapling output.
#[derive(Debug)]
pub struct Output {
    pub cmu: Vec<u8>,
    pub ephemeral_key: Vec<u8>,
    pub enc_ciphertext: Vec<u8>,
}

#[derive(Debug)]
pub struct JoinSplit {
    // Define fields based on your needs
}

/// Orchard actions.
#[derive(Debug)]
pub struct Action {
    pub nullifier: Vec<u8>,
    pub cmx: Vec<u8>,
    pub ephemeral_key: Vec<u8>,
    pub enc_ciphertext: Vec<u8>,
}

/// Raw transactrion.
#[derive(Debug)]
pub struct TransactionData {
    pub f_overwintered: bool,
    pub version: u32,
    pub n_version_group_id: u32,
    pub consensus_branch_id: u32,
    pub transparent_inputs: Vec<TxIn>,
    pub transparent_outputs: Vec<TxOut>,
    pub shielded_spends: Vec<Spend>,
    pub shielded_outputs: Vec<Output>,
    pub join_splits: Vec<JoinSplit>,
    pub orchard_actions: Vec<Action>,
}

/// Zingo-Proxy transaction data.
#[derive(Debug)]
pub struct FullTransaction {
    pub raw_transaction: TransactionData,
    pub raw_bytes: Vec<u8>,
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
