use std::sync::Arc;

use chrono::{DateTime, Utc};
use derive_getters::Getters;
use derive_new::new;
use hex::ToHex as _;
use zebra_chain::{
    amount::{Amount, NegativeAllowed},
    block::{self, Height},
    orchard,
    parameters::Network,
    primitives::ed25519,
    sapling::ValueCommitment,
    serialization::ZcashSerialize as _,
    transaction::{self, SerializedTransaction, Transaction},
    transparent::Script,
};

use zcash_script::script::Asm as _;

use super::hex::{arrayhex, opthex};
use super::zec::Zec;

/// A transaction object as returned by the `getrawtransaction` and `getblock` requests.
#[allow(clippy::too_many_arguments)]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct TransactionObject {
    /// Whether the containing block is in the best chain, present only when a block is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    in_active_chain: Option<bool>,
    /// The raw transaction, encoded as hex bytes.
    #[serde(with = "hex")]
    hex: SerializedTransaction,
    /// The containing block's height in the best chain, -1 in a side chain, or `None` in the mempool.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    height: Option<i32>,
    /// The confirmations of the containing block, 0 in a side chain, or `None` in the mempool.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    confirmations: Option<i64>,

    /// Transparent inputs of the transaction.
    #[serde(rename = "vin")]
    inputs: Vec<Input>,

    /// Transparent outputs of the transaction.
    #[serde(rename = "vout")]
    outputs: Vec<Output>,

    /// Sapling spends of the transaction.
    #[serde(rename = "vShieldedSpend")]
    shielded_spends: Vec<ShieldedSpend>,

    /// Sapling outputs of the transaction.
    #[serde(rename = "vShieldedOutput")]
    shielded_outputs: Vec<ShieldedOutput>,

    /// Sprout JoinSplits of the transaction.
    #[serde(rename = "vjoinsplit")]
    joinsplits: Vec<JoinSplit>,

    /// Sapling binding signature of the transaction.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "opthex",
        default,
        rename = "bindingSig"
    )]
    #[getter(copy)]
    binding_sig: Option<[u8; 64]>,

    /// JoinSplit public key of the transaction.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "opthex",
        default,
        rename = "joinSplitPubKey"
    )]
    #[getter(copy)]
    joinsplit_pub_key: Option<[u8; 32]>,

    /// JoinSplit signature of the transaction.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "opthex",
        default,
        rename = "joinSplitSig"
    )]
    #[getter(copy)]
    joinsplit_sig: Option<[u8; ed25519::Signature::BYTE_SIZE]>,

    /// Orchard actions of the transaction.
    #[serde(rename = "orchard", skip_serializing_if = "Option::is_none")]
    orchard: Option<Orchard>,

    /// Ironwood actions of the transaction, in the Orchard bundle's shape, for v6 transactions from NU6.3 onward.
    #[serde(rename = "ironwood", skip_serializing_if = "Option::is_none")]
    ironwood: Option<Orchard>,

    /// The net value of Sapling spends minus outputs, in ZEC.
    #[serde(rename = "valueBalance", skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    value_balance: Option<f64>,

    /// The net value of Sapling spends minus outputs, in zatoshis.
    #[serde(rename = "valueBalanceZat", skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    value_balance_zat: Option<i64>,

    /// The size of the transaction in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    size: Option<i64>,

    /// The time the transaction was included in a block.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    time: Option<i64>,

    /// The transaction identifier, encoded as hex bytes.
    #[serde(with = "hex")]
    #[getter(copy)]
    txid: transaction::Hash,

    /// The transaction's auth digest, all `ff` bytes for transactions before v5.
    #[serde(
        rename = "authdigest",
        with = "opthex",
        skip_serializing_if = "Option::is_none",
        default
    )]
    #[getter(copy)]
    auth_digest: Option<transaction::AuthDigest>,

    /// Whether the overwintered flag is set.
    overwintered: bool,

    /// The version of the transaction.
    version: u32,

    /// The version group ID.
    #[serde(
        rename = "versiongroupid",
        with = "opthex",
        skip_serializing_if = "Option::is_none",
        default
    )]
    version_group_id: Option<Vec<u8>>,

    /// The lock time.
    #[serde(rename = "locktime")]
    lock_time: u32,

    /// The height after which the transaction expires, present only for Overwinter and later transactions as in zcashd.
    #[serde(rename = "expiryheight", skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    expiry_height: Option<Height>,

    /// The hash of the block that contains the transaction.
    #[serde(
        rename = "blockhash",
        with = "opthex",
        skip_serializing_if = "Option::is_none",
        default
    )]
    #[getter(copy)]
    block_hash: Option<block::Hash>,

    /// The time of the block that contains the transaction.
    #[serde(rename = "blocktime", skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    block_time: Option<i64>,
}

/// The transparent input of a transaction.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Input {
    /// A coinbase input.
    Coinbase {
        /// The coinbase scriptSig as hex.
        #[serde(with = "hex")]
        coinbase: Vec<u8>,
        /// The script sequence number.
        sequence: u32,
    },
    /// A non-coinbase input.
    NonCoinbase {
        /// The transaction id.
        txid: String,
        /// The vout index.
        vout: u32,
        /// The script.
        #[serde(rename = "scriptSig")]
        script_sig: ScriptSig,
        /// The script sequence number.
        sequence: u32,
        /// The value of the output being spent in ZEC.
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<f64>,
        /// The value of the output being spent, in zats, named to match zcashd.
        #[serde(rename = "valueSat", skip_serializing_if = "Option::is_none")]
        value_zat: Option<i64>,
        /// The address of the output being spent.
        #[serde(skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
}

/// The transparent output of a transaction.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct Output {
    /// The value in ZEC.
    value: f64,
    /// The value in zats.
    #[serde(rename = "valueZat")]
    value_zat: i64,
    /// The output index.
    n: u32,
    /// The scriptPubKey.
    #[serde(rename = "scriptPubKey")]
    script_pub_key: ScriptPubKey,
}

/// The scriptPubKey of a transaction output.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct ScriptPubKey {
    /// the asm.
    asm: String,
    /// the hex.
    #[serde(with = "hex")]
    hex: Script,
    /// The required sigs.
    #[serde(rename = "reqSigs")]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    req_sigs: Option<u32>,
    /// The type, eg 'pubkeyhash'.
    r#type: String,
    /// The addresses.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    addresses: Option<Vec<String>>,
}

/// The scriptSig of a transaction input.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct ScriptSig {
    /// The asm.
    asm: String,
    /// The hex.
    hex: Script,
}

/// A Sprout JoinSplit of a transaction.
#[allow(clippy::too_many_arguments)]
#[serde_with::serde_as]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct JoinSplit {
    /// Public input value in ZEC.
    #[serde(rename = "vpub_old")]
    old_public_value: f64,
    /// Public input value in zatoshis.
    #[serde(rename = "vpub_oldZat")]
    old_public_value_zat: i64,
    /// Public output value in ZEC.
    #[serde(rename = "vpub_new")]
    new_public_value: f64,
    /// Public output value in zatoshis.
    #[serde(rename = "vpub_newZat")]
    new_public_value_zat: i64,
    /// Merkle root of the Sprout note commitment tree.
    #[serde(with = "hex")]
    #[getter(copy)]
    anchor: [u8; 32],
    /// The nullifier of the input notes.
    #[serde_as(as = "Vec<serde_with::hex::Hex>")]
    nullifiers: Vec<[u8; 32]>,
    /// The commitments of the output notes.
    #[serde_as(as = "Vec<serde_with::hex::Hex>")]
    commitments: Vec<[u8; 32]>,
    /// The one-time public key used to encrypt the ciphertexts.
    #[serde(rename = "onetimePubKey")]
    #[serde(with = "hex")]
    #[getter(copy)]
    one_time_pubkey: [u8; 32],
    /// The random seed.
    #[serde(rename = "randomSeed")]
    #[serde(with = "hex")]
    #[getter(copy)]
    random_seed: [u8; 32],
    /// The input notes MACs.
    #[serde_as(as = "Vec<serde_with::hex::Hex>")]
    macs: Vec<[u8; 32]>,
    /// A zero-knowledge proof using the Sprout circuit.
    #[serde(with = "hex")]
    proof: Vec<u8>,
    /// The output notes ciphertexts.
    #[serde_as(as = "Vec<serde_with::hex::Hex>")]
    ciphertexts: Vec<Vec<u8>>,
}

/// A Sapling spend of a transaction.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct ShieldedSpend {
    /// Value commitment to the input note.
    #[serde(with = "hex")]
    #[getter(skip)]
    cv: ValueCommitment,
    /// Merkle root of the Sapling note commitment tree.
    #[serde(with = "hex")]
    #[getter(copy)]
    anchor: [u8; 32],
    /// The nullifier of the input note.
    #[serde(with = "hex")]
    #[getter(copy)]
    nullifier: [u8; 32],
    /// The randomized public key for spendAuthSig.
    #[serde(with = "hex")]
    #[getter(copy)]
    rk: [u8; 32],
    /// A zero-knowledge proof using the Sapling Spend circuit.
    #[serde(with = "hex")]
    #[getter(copy)]
    proof: [u8; 192],
    /// A signature authorizing this Spend.
    #[serde(rename = "spendAuthSig", with = "hex")]
    #[getter(copy)]
    spend_auth_sig: [u8; 64],
}

impl ShieldedSpend {
    /// The value commitment to the input note.
    pub fn cv(&self) -> ValueCommitment {
        self.cv.clone()
    }
}

/// A Sapling output of a transaction.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct ShieldedOutput {
    /// Value commitment to the output note.
    #[serde(with = "hex")]
    #[getter(skip)]
    cv: ValueCommitment,
    /// The u-coordinate of the note commitment for the output note.
    #[serde(rename = "cmu", with = "hex")]
    cm_u: [u8; 32],
    /// A Jubjub public key.
    #[serde(rename = "ephemeralKey", with = "hex")]
    ephemeral_key: [u8; 32],
    /// The output note encrypted to the recipient.
    #[serde(rename = "encCiphertext", with = "arrayhex")]
    enc_ciphertext: [u8; 580],
    /// A ciphertext enabling the sender to recover the output note.
    #[serde(rename = "outCiphertext", with = "hex")]
    out_ciphertext: [u8; 80],
    /// A zero-knowledge proof using the Sapling Output circuit.
    #[serde(with = "hex")]
    proof: [u8; 192],
}

impl ShieldedOutput {
    /// The value commitment to the output note.
    pub fn cv(&self) -> ValueCommitment {
        self.cv.clone()
    }
}

/// Object with Orchard-specific information.
#[serde_with::serde_as]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct Orchard {
    /// Array of Orchard actions.
    actions: Vec<OrchardAction>,
    /// The net value of Orchard Actions in ZEC.
    #[serde(rename = "valueBalance")]
    value_balance: f64,
    /// The net value of Orchard Actions in zatoshis.
    #[serde(rename = "valueBalanceZat")]
    value_balance_zat: i64,
    /// The flags.
    #[serde(skip_serializing_if = "Option::is_none")]
    flags: Option<OrchardFlags>,
    /// A root of the Orchard note commitment tree at some past block height.
    #[serde_as(as = "Option<serde_with::hex::Hex>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[getter(copy)]
    anchor: Option<[u8; 32]>,
    /// The aggregated zk-SNARK proof for the Orchard actions.
    #[serde_as(as = "Option<serde_with::hex::Hex>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    proof: Option<Vec<u8>>,
    /// An Orchard binding signature on the SIGHASH transaction hash.
    #[serde(rename = "bindingSig")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde_as(as = "Option<serde_with::hex::Hex>")]
    #[getter(copy)]
    binding_sig: Option<[u8; 64]>,
}

/// The flags of an Orchard-shaped bundle.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct OrchardFlags {
    /// Whether Orchard outputs are enabled.
    #[serde(rename = "enableOutputs")]
    enable_outputs: bool,
    /// Whether Orchard spends are enabled.
    #[serde(rename = "enableSpends")]
    enable_spends: bool,
}

/// The Orchard action of a transaction.
#[allow(clippy::too_many_arguments)]
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Getters, new)]
pub struct OrchardAction {
    /// A value commitment to the net value of the input note minus the output note.
    #[serde(with = "hex")]
    cv: [u8; 32],
    /// The nullifier of the input note.
    #[serde(with = "hex")]
    nullifier: [u8; 32],
    /// The randomized validating key for spendAuthSig.
    #[serde(with = "hex")]
    rk: [u8; 32],
    /// The x-coordinate of the note commitment for the output note.
    #[serde(rename = "cmx", with = "hex")]
    cm_x: [u8; 32],
    /// An encoding of an ephemeral Pallas public key.
    #[serde(rename = "ephemeralKey", with = "hex")]
    ephemeral_key: [u8; 32],
    /// The output note encrypted to the recipient.
    #[serde(rename = "encCiphertext", with = "arrayhex")]
    enc_ciphertext: [u8; 580],
    /// A signature authorizing the spend in this action.
    #[serde(rename = "spendAuthSig", with = "hex")]
    spend_auth_sig: [u8; 64],
    /// A ciphertext enabling the sender to recover the output note.
    #[serde(rename = "outCiphertext", with = "hex")]
    out_ciphertext: [u8; 80],
}

/// Builds the [`Orchard`] object for an Orchard-shaped pool, Orchard or Ironwood, from its shielded data and net value balance.
fn orchard_shaped_object(
    shielded_data: Option<&orchard::ShieldedData>,
    value_balance: Amount<NegativeAllowed>,
) -> Orchard {
    let actions = shielded_data
        .into_iter()
        .flat_map(|data| data.actions.iter())
        .map(|authorized_action| {
            let action = &authorized_action.action;
            OrchardAction {
                cv: action.cv.into(),
                nullifier: action.nullifier.into(),
                rk: action.rk.into(),
                cm_x: action.cm_x.into(),
                ephemeral_key: action.ephemeral_key.into(),
                enc_ciphertext: action.enc_ciphertext.into(),
                spend_auth_sig: authorized_action.spend_auth_sig.into(),
                out_ciphertext: action.out_ciphertext.into(),
            }
        })
        .collect();

    Orchard {
        actions,
        value_balance: Zec::from(value_balance).lossy_zec(),
        value_balance_zat: value_balance.zatoshis(),
        flags: shielded_data.map(|data| {
            OrchardFlags::new(
                data.flags.contains(orchard::Flags::ENABLE_OUTPUTS),
                data.flags.contains(orchard::Flags::ENABLE_SPENDS),
            )
        }),
        anchor: shielded_data.map(|data| data.shared_anchor.bytes_in_display_order()),
        proof: shielded_data.map(|data| data.proof.bytes_in_display_order()),
        binding_sig: shielded_data.map(|data| data.binding_sig.into()),
    }
}

impl TransactionObject {
    /// Renders `tx` as the verbose transaction object, placed in the chain by its block facts.
    #[allow(clippy::too_many_arguments)]
    pub fn from_transaction(
        tx: Arc<Transaction>,
        height: Option<block::Height>,
        confirmations: Option<i64>,
        network: &Network,
        block_time: Option<DateTime<Utc>>,
        block_hash: Option<block::Hash>,
        in_active_chain: Option<bool>,
        txid: transaction::Hash,
    ) -> Self {
        let block_time = block_time.map(|bt| bt.timestamp());
        Self {
            hex: tx.clone().into(),
            height: if in_active_chain.unwrap_or_default() {
                height.map(|height| height.0 as i32)
            } else if block_hash.is_some() {
                // Side chain
                Some(-1)
            } else {
                // Mempool
                None
            },
            confirmations: if in_active_chain.unwrap_or_default() {
                confirmations
            } else if block_hash.is_some() {
                // Side chain
                Some(0)
            } else {
                // Mempool
                None
            },
            inputs: tx
                .inputs()
                .iter()
                .map(|input| match input {
                    zebra_chain::transparent::Input::Coinbase { sequence, .. } => Input::Coinbase {
                        coinbase: input
                            .coinbase_script()
                            .expect("we know it is a valid coinbase script"),
                        sequence: *sequence,
                    },
                    zebra_chain::transparent::Input::PrevOut {
                        sequence,
                        unlock_script,
                        outpoint,
                    } => Input::NonCoinbase {
                        txid: outpoint.hash.encode_hex(),
                        vout: outpoint.index,
                        script_sig: ScriptSig {
                            // https://github.com/zcash/zcash/blob/v6.11.0/src/rpc/rawtransaction.cpp#L240
                            asm: zcash_script::script::Code(unlock_script.as_raw_bytes().to_vec())
                                .to_asm(true),
                            hex: unlock_script.clone(),
                        },
                        sequence: *sequence,
                        value: None,
                        value_zat: None,
                        address: None,
                    },
                })
                .collect(),
            outputs: tx
                .outputs()
                .iter()
                .enumerate()
                .map(|output| {
                    // Parse the scriptPubKey to find destination addresses.
                    let (addresses, req_sigs) = output
                        .1
                        .address(network)
                        .map(|address| (vec![address.to_string()], 1))
                        .unzip();

                    Output {
                        value: Zec::from(output.1.value).lossy_zec(),
                        value_zat: output.1.value.zatoshis(),
                        n: output.0 as u32,
                        script_pub_key: ScriptPubKey {
                            // https://github.com/zcash/zcash/blob/v6.11.0/src/rpc/rawtransaction.cpp#L271
                            // https://github.com/zcash/zcash/blob/v6.11.0/src/rpc/rawtransaction.cpp#L45
                            asm: zcash_script::script::Code(
                                output.1.lock_script.as_raw_bytes().to_vec(),
                            )
                            .to_asm(false),
                            hex: output.1.lock_script.clone(),
                            req_sigs,
                            r#type: zcash_script::script::Code(
                                output.1.lock_script.as_raw_bytes().to_vec(),
                            )
                            .to_component()
                            .ok()
                            .and_then(|c| c.refine().ok())
                            .and_then(|component| zcash_script::solver::standard(&component))
                            .map(|kind| match kind {
                                zcash_script::solver::ScriptKind::PubKeyHash { .. } => "pubkeyhash",
                                zcash_script::solver::ScriptKind::ScriptHash { .. } => "scripthash",
                                zcash_script::solver::ScriptKind::MultiSig { .. } => "multisig",
                                zcash_script::solver::ScriptKind::NullData { .. } => "nulldata",
                                zcash_script::solver::ScriptKind::PubKey { .. } => "pubkey",
                            })
                            .unwrap_or("nonstandard")
                            .to_string(),
                            addresses,
                        },
                    }
                })
                .collect(),
            shielded_spends: tx
                .sapling_spends_per_anchor()
                .map(|spend| {
                    let mut anchor = <[u8; 32]>::from(&spend.per_spend_anchor);
                    anchor.reverse();

                    let mut nullifier = <[u8; 32]>::from(spend.nullifier);
                    nullifier.reverse();

                    let mut rk: [u8; 32] = spend.clone().rk.into();
                    rk.reverse();

                    let spend_auth_sig: [u8; 64] = spend.spend_auth_sig.into();

                    ShieldedSpend {
                        cv: spend.cv.clone(),
                        anchor,
                        nullifier,
                        rk,
                        proof: spend.zkproof.0,
                        spend_auth_sig,
                    }
                })
                .collect(),
            shielded_outputs: tx
                .sapling_outputs()
                .map(|output| {
                    let mut cm_u: [u8; 32] = output.cm_u.to_bytes();
                    cm_u.reverse();
                    let mut ephemeral_key: [u8; 32] = output.ephemeral_key.into();
                    ephemeral_key.reverse();
                    let enc_ciphertext: [u8; 580] = output.enc_ciphertext.into();
                    let out_ciphertext: [u8; 80] = output.out_ciphertext.into();

                    ShieldedOutput {
                        cv: output.cv.clone(),
                        cm_u,
                        ephemeral_key,
                        enc_ciphertext,
                        out_ciphertext,
                        proof: output.zkproof.0,
                    }
                })
                .collect(),
            joinsplits: tx
                .sprout_joinsplits()
                .map(|joinsplit| {
                    let mut ephemeral_key_bytes: [u8; 32] = joinsplit.ephemeral_key.to_bytes();
                    ephemeral_key_bytes.reverse();

                    JoinSplit {
                        old_public_value: Zec::from(joinsplit.vpub_old).lossy_zec(),
                        old_public_value_zat: joinsplit.vpub_old.zatoshis(),
                        new_public_value: Zec::from(joinsplit.vpub_new).lossy_zec(),
                        new_public_value_zat: joinsplit.vpub_new.zatoshis(),
                        anchor: joinsplit.anchor.bytes_in_display_order(),
                        nullifiers: joinsplit
                            .nullifiers
                            .iter()
                            .map(|n| n.bytes_in_display_order())
                            .collect(),
                        commitments: joinsplit
                            .commitments
                            .iter()
                            .map(|c| c.bytes_in_display_order())
                            .collect(),
                        one_time_pubkey: ephemeral_key_bytes,
                        random_seed: joinsplit.random_seed.bytes_in_display_order(),
                        macs: joinsplit
                            .vmacs
                            .iter()
                            .map(|m| m.bytes_in_display_order())
                            .collect(),
                        proof: joinsplit.zkproof.unwrap_or_default(),
                        ciphertexts: joinsplit
                            .enc_ciphertexts
                            .iter()
                            .map(|c| c.zcash_serialize_to_vec().unwrap_or_default())
                            .collect(),
                    }
                })
                .collect(),
            value_balance: Some(Zec::from(tx.sapling_value_balance().sapling_amount()).lossy_zec()),
            value_balance_zat: Some(tx.sapling_value_balance().sapling_amount().zatoshis()),
            orchard: Some(orchard_shaped_object(
                tx.orchard_shielded_data(),
                tx.orchard_value_balance().orchard_amount(),
            )),
            ironwood: tx.ironwood_shielded_data().map(|data| {
                orchard_shaped_object(Some(data), tx.ironwood_value_balance().ironwood_amount())
            }),
            binding_sig: tx.sapling_binding_sig().map(|raw_sig| raw_sig.into()),
            joinsplit_pub_key: tx.joinsplit_pub_key().map(|raw_key| {
                // Display order is reversed in the RPC output.
                let mut key: [u8; 32] = raw_key.into();
                key.reverse();
                key
            }),
            joinsplit_sig: tx.joinsplit_sig().map(|raw_sig| raw_sig.into()),
            size: tx
                .zcash_serialize_to_vec()
                .ok()
                .and_then(|bytes| bytes.len().try_into().ok()),
            time: block_time,
            txid,
            in_active_chain,
            auth_digest: tx.auth_digest(),
            overwintered: tx.is_overwintered(),
            version: tx.version(),
            version_group_id: tx.version_group_id().map(|id| id.to_be_bytes().to_vec()),
            lock_time: tx.raw_lock_time(),
            // zcashd includes expiryheight only for Overwinter+ transactions.
            // For those, expiry_height of 0 means "no expiry" per ZIP-203.
            expiry_height: if tx.is_overwintered() {
                Some(tx.expiry_height().unwrap_or(Height(0)))
            } else {
                None
            },
            block_hash,
            block_time,
        }
    }
}

/// A response to a `getrawtransaction` request: the raw transaction, or its object when verbose.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum GetRawTransaction {
    /// The raw transaction, encoded as hex bytes.
    Raw(#[serde(with = "hex")] SerializedTransaction),
    /// The transaction object.
    Object(Box<TransactionObject>),
}
