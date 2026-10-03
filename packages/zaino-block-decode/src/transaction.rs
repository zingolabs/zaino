//! Walking one transaction's consensus encoding, every version from v1 to v6,
//! keeping only what the projection and the transaction id need.
//!
//! Shielded fields stay bytes: a nullifier, a note commitment, an ephemeral
//! key and a ciphertext are copied or borrowed as they are on the wire, and
//! value commitments, proofs and signatures are stepped over by size. No curve
//! point is decompressed and no field element is reduced, which is where a
//! full deserialiser spends most of its time on shielded-heavy blocks.
//!
//! The fields kept are exactly the union of what the domain transaction holds
//! and what the ZIP-244 id commits to, so the walk is one pass.

use crate::error::DecodeError;
use crate::reader::Reader;

const OVERWINTER_FLAG: u32 = 1 << 31;
const V3_VERSION_GROUP_ID: u32 = 0x03C4_8270;
const V4_VERSION_GROUP_ID: u32 = 0x892F_2085;
const V5_VERSION_GROUP_ID: u32 = 0x26A7_270A;
const V6_VERSION_GROUP_ID: u32 = 0xD884_B698;

/// Groth16 proof, as in Sapling and post-Sapling Sprout.
const GROTH_PROOF: usize = 192;
/// PHGR13 proof, as in Sprout before Sapling.
const PHGR_PROOF: usize = 296;
const SIGNATURE: usize = 64;
/// A Sprout JoinSplit minus its proof: `vpub_old`, `vpub_new`, anchor, two
/// nullifiers, two commitments, ephemeral key, random seed, two MACs, two
/// 601-byte note ciphertexts.
const JOINSPLIT_WITHOUT_PROOF: usize = 8 + 8 + 32 + 64 + 64 + 32 + 32 + 64 + 2 * 601;
/// A full Sapling or Orchard note ciphertext.
pub(crate) const ENC_CIPHERTEXT: usize = 580;
/// The outgoing ciphertext beside it.
const OUT_CIPHERTEXT: usize = 80;

/// The fewest bytes each item of a vector occupies, for the pre-allocation bound.
const MIN_INPUT: usize = 32 + 4 + 1 + 4;
const MIN_OUTPUT: usize = 8 + 1;
const SAPLING_SPEND_V4: usize = 32 + 32 + 32 + 32 + GROTH_PROOF + SIGNATURE;
const SAPLING_OUTPUT_V4: usize = 32 + 32 + 32 + ENC_CIPHERTEXT + OUT_CIPHERTEXT + GROTH_PROOF;
const SAPLING_SPEND_V5: usize = 32 + 32 + 32;
const SAPLING_OUTPUT_V5: usize = 32 + 32 + 32 + ENC_CIPHERTEXT + OUT_CIPHERTEXT;
const ORCHARD_ACTION: usize = 32 * 5 + ENC_CIPHERTEXT + OUT_CIPHERTEXT;

/// The transaction formats, by the version word that opens each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Version {
    /// Pre-Overwinter: version 1 (transparent only) or 2 (with JoinSplits).
    Sprout(u32),
    V3,
    V4,
    V5,
    V6,
}

impl Version {
    fn has_sprout(self) -> bool {
        match self {
            Version::Sprout(v) => v >= 2,
            Version::V3 | Version::V4 => true,
            Version::V5 | Version::V6 => false,
        }
    }

    /// The version word as encoded, overwinter flag included.
    pub(crate) fn header_word(self) -> u32 {
        match self {
            Version::Sprout(v) => v,
            Version::V3 => OVERWINTER_FLAG | 3,
            Version::V4 => OVERWINTER_FLAG | 4,
            Version::V5 => OVERWINTER_FLAG | 5,
            Version::V6 => OVERWINTER_FLAG | 6,
        }
    }

    pub(crate) fn version_group_id(self) -> u32 {
        match self {
            Version::Sprout(_) => 0,
            Version::V3 => V3_VERSION_GROUP_ID,
            Version::V4 => V4_VERSION_GROUP_ID,
            Version::V5 => V5_VERSION_GROUP_ID,
            Version::V6 => V6_VERSION_GROUP_ID,
        }
    }
}

pub(crate) struct RawInput<'a> {
    pub(crate) prevout_hash: [u8; 32],
    pub(crate) prevout_index: u32,
    pub(crate) script_sig: &'a [u8],
    pub(crate) sequence: u32,
}

impl RawInput<'_> {
    /// A coinbase input spends nothing: the null outpoint.
    pub(crate) fn is_coinbase(&self) -> bool {
        self.prevout_hash == [0u8; 32] && self.prevout_index == u32::MAX
    }
}

pub(crate) struct RawOutput<'a> {
    /// The value as encoded, a little-endian signed 64-bit amount.
    pub(crate) value: [u8; 8],
    pub(crate) script: &'a [u8],
}

pub(crate) struct RawSaplingSpend {
    pub(crate) cv: [u8; 32],
    pub(crate) nullifier: [u8; 32],
    pub(crate) rk: [u8; 32],
}

pub(crate) struct RawSaplingOutput<'a> {
    pub(crate) cv: [u8; 32],
    pub(crate) cmu: [u8; 32],
    pub(crate) ephemeral_key: [u8; 32],
    pub(crate) enc_ciphertext: &'a [u8],
    pub(crate) out_ciphertext: &'a [u8],
}

#[derive(Default)]
pub(crate) struct RawSapling<'a> {
    pub(crate) spends: Vec<RawSaplingSpend>,
    pub(crate) outputs: Vec<RawSaplingOutput<'a>>,
    pub(crate) value_balance: i64,
    /// The bundle anchor, present when there are spends (v5 and v6 encode it
    /// once per bundle; v4 encodes it per spend, where the id does not need it).
    pub(crate) anchor: Option<[u8; 32]>,
}

impl RawSapling<'_> {
    pub(crate) fn is_empty(&self) -> bool {
        self.spends.is_empty() && self.outputs.is_empty()
    }
}

pub(crate) struct RawAction<'a> {
    pub(crate) cv: [u8; 32],
    pub(crate) nullifier: [u8; 32],
    pub(crate) rk: [u8; 32],
    pub(crate) cmx: [u8; 32],
    pub(crate) ephemeral_key: [u8; 32],
    pub(crate) enc_ciphertext: &'a [u8],
    pub(crate) out_ciphertext: &'a [u8],
}

/// An Orchard-shaped bundle: the Orchard pool in v5 and v6, the Ironwood pool
/// in v6. Absent when the transaction has no actions in that pool.
pub(crate) struct RawOrchardBundle<'a> {
    pub(crate) actions: Vec<RawAction<'a>>,
    pub(crate) flags: u8,
    pub(crate) value_balance: i64,
    pub(crate) anchor: [u8; 32],
}

pub(crate) struct RawTransaction<'a> {
    pub(crate) version: Version,
    /// Zero for v1–v4, which do not encode one.
    pub(crate) consensus_branch_id: u32,
    pub(crate) lock_time: u32,
    pub(crate) expiry_height: u32,
    pub(crate) inputs: Vec<RawInput<'a>>,
    pub(crate) outputs: Vec<RawOutput<'a>>,
    pub(crate) sapling: RawSapling<'a>,
    pub(crate) orchard: Option<RawOrchardBundle<'a>>,
    pub(crate) ironwood: Option<RawOrchardBundle<'a>>,
    /// The whole encoding, which is what a v1–v4 id hashes.
    pub(crate) bytes: &'a [u8],
}

pub(crate) fn read_transaction<'a>(
    reader: &mut Reader<'a>,
) -> Result<RawTransaction<'a>, DecodeError> {
    let start = reader.position();
    let header = reader.u32_le()?;
    let version = if header & OVERWINTER_FLAG != 0 {
        let group = reader.u32_le()?;
        match (header & !OVERWINTER_FLAG, group) {
            (3, V3_VERSION_GROUP_ID) => Version::V3,
            (4, V4_VERSION_GROUP_ID) => Version::V4,
            (5, V5_VERSION_GROUP_ID) => Version::V5,
            (6, V6_VERSION_GROUP_ID) => Version::V6,
            _ => {
                return Err(DecodeError::UnknownTransactionVersion {
                    header,
                    group: Some(group),
                });
            }
        }
    } else {
        match header {
            1 | 2 => Version::Sprout(header),
            _ => {
                return Err(DecodeError::UnknownTransactionVersion {
                    header,
                    group: None,
                });
            }
        }
    };
    let mut transaction = match version {
        Version::Sprout(_) | Version::V3 | Version::V4 => read_v4_format(reader, version)?,
        Version::V5 => read_v5_format(reader, version, false)?,
        Version::V6 => read_v5_format(reader, version, true)?,
    };
    transaction.bytes = reader.since(start);
    Ok(transaction)
}

/// The v1–v4 layout: transparent, lock time, expiry (v3+), Sapling (v4),
/// JoinSplits (v2+), binding signature (v4 with Sapling data).
fn read_v4_format<'a>(
    reader: &mut Reader<'a>,
    version: Version,
) -> Result<RawTransaction<'a>, DecodeError> {
    let inputs = reader.vector(MIN_INPUT, read_input)?;
    let outputs = reader.vector(MIN_OUTPUT, read_output)?;
    let lock_time = reader.u32_le()?;
    let expiry_height = match version {
        Version::Sprout(_) => 0,
        Version::V3 | Version::V4 | Version::V5 | Version::V6 => reader.u32_le()?,
    };
    let sapling = match version {
        Version::V4 => read_sapling_v4(reader)?,
        Version::Sprout(_) | Version::V3 | Version::V5 | Version::V6 => RawSapling::default(),
    };
    if version.has_sprout() {
        skip_joinsplits(reader, version == Version::V4)?;
    }
    if version == Version::V4 && !sapling.is_empty() {
        reader.skip(SIGNATURE)?;
    }
    Ok(RawTransaction {
        version,
        consensus_branch_id: 0,
        lock_time,
        expiry_height,
        inputs,
        outputs,
        sapling,
        orchard: None,
        ironwood: None,
        bytes: &[],
    })
}

/// The v5 layout, and v6 which appends an Ironwood bundle to it: header
/// fragment, transparent, Sapling, Orchard, then Ironwood.
fn read_v5_format<'a>(
    reader: &mut Reader<'a>,
    version: Version,
    with_ironwood: bool,
) -> Result<RawTransaction<'a>, DecodeError> {
    let consensus_branch_id = reader.u32_le()?;
    let lock_time = reader.u32_le()?;
    let expiry_height = reader.u32_le()?;
    let inputs = reader.vector(MIN_INPUT, read_input)?;
    let outputs = reader.vector(MIN_OUTPUT, read_output)?;
    let sapling = read_sapling_v5(reader)?;
    let orchard = read_orchard_bundle(reader)?;
    let ironwood = if with_ironwood {
        read_orchard_bundle(reader)?
    } else {
        None
    };
    Ok(RawTransaction {
        version,
        consensus_branch_id,
        lock_time,
        expiry_height,
        inputs,
        outputs,
        sapling,
        orchard,
        ironwood,
        bytes: &[],
    })
}

fn read_input<'a>(reader: &mut Reader<'a>) -> Result<RawInput<'a>, DecodeError> {
    Ok(RawInput {
        prevout_hash: reader.array()?,
        prevout_index: reader.u32_le()?,
        script_sig: reader.var_bytes()?,
        sequence: reader.u32_le()?,
    })
}

fn read_output<'a>(reader: &mut Reader<'a>) -> Result<RawOutput<'a>, DecodeError> {
    Ok(RawOutput {
        value: reader.array()?,
        script: reader.var_bytes()?,
    })
}

/// v4 Sapling: value balance, then spends and outputs each carrying their own
/// anchor, proof and signature inline.
fn read_sapling_v4<'a>(reader: &mut Reader<'a>) -> Result<RawSapling<'a>, DecodeError> {
    let value_balance = reader.i64_le()?;
    let spends = reader.vector(SAPLING_SPEND_V4, |r| {
        let cv = r.array()?;
        r.skip(32)?; // anchor, repeated per spend
        let nullifier = r.array()?;
        let rk = r.array()?;
        r.skip(GROTH_PROOF + SIGNATURE)?;
        Ok(RawSaplingSpend { cv, nullifier, rk })
    })?;
    let outputs = reader.vector(SAPLING_OUTPUT_V4, |r| {
        let output = read_sapling_output_fields(r)?;
        r.skip(GROTH_PROOF)?;
        Ok(output)
    })?;
    Ok(RawSapling {
        spends,
        outputs,
        value_balance,
        anchor: None,
    })
}

/// v5 Sapling (also v6): spends and outputs without their authorising data,
/// then the value balance, the bundle anchor, and the proofs and signatures
/// as separate arrays.
fn read_sapling_v5<'a>(reader: &mut Reader<'a>) -> Result<RawSapling<'a>, DecodeError> {
    let spends = reader.vector(SAPLING_SPEND_V5, |r| {
        Ok(RawSaplingSpend {
            cv: r.array()?,
            nullifier: r.array()?,
            rk: r.array()?,
        })
    })?;
    let outputs = reader.vector(SAPLING_OUTPUT_V5, read_sapling_output_fields)?;
    let any = !(spends.is_empty() && outputs.is_empty());
    let value_balance = if any { reader.i64_le()? } else { 0 };
    let anchor = if spends.is_empty() {
        None
    } else {
        Some(reader.array()?)
    };
    reader.skip(spends.len() * (GROTH_PROOF + SIGNATURE))?;
    reader.skip(outputs.len() * GROTH_PROOF)?;
    if any {
        reader.skip(SIGNATURE)?;
    }
    Ok(RawSapling {
        spends,
        outputs,
        value_balance,
        anchor,
    })
}

/// The output fields v4 and v5 share: `cv`, `cmu`, ephemeral key, note
/// ciphertext, outgoing ciphertext.
fn read_sapling_output_fields<'a>(
    reader: &mut Reader<'a>,
) -> Result<RawSaplingOutput<'a>, DecodeError> {
    Ok(RawSaplingOutput {
        cv: reader.array()?,
        cmu: reader.array()?,
        ephemeral_key: reader.array()?,
        enc_ciphertext: reader.take(ENC_CIPHERTEXT)?,
        out_ciphertext: reader.take(OUT_CIPHERTEXT)?,
    })
}

/// An Orchard-shaped bundle: actions without their signatures, then — only
/// when there are any — flags, value balance, anchor, the proof, the spend
/// authorisations and the binding signature.
fn read_orchard_bundle<'a>(
    reader: &mut Reader<'a>,
) -> Result<Option<RawOrchardBundle<'a>>, DecodeError> {
    let actions = reader.vector(ORCHARD_ACTION, |r| {
        Ok(RawAction {
            cv: r.array()?,
            nullifier: r.array()?,
            rk: r.array()?,
            cmx: r.array()?,
            ephemeral_key: r.array()?,
            enc_ciphertext: r.take(ENC_CIPHERTEXT)?,
            out_ciphertext: r.take(OUT_CIPHERTEXT)?,
        })
    })?;
    if actions.is_empty() {
        return Ok(None);
    }
    let flags = reader.u8()?;
    let value_balance = reader.i64_le()?;
    let anchor = reader.array()?;
    reader.var_bytes()?; // proof
    reader.skip(actions.len() * SIGNATURE)?;
    reader.skip(SIGNATURE)?;
    Ok(Some(RawOrchardBundle {
        actions,
        flags,
        value_balance,
        anchor,
    }))
}

/// Step over the JoinSplits and, when there are any, the JoinSplit public key
/// and signature. Nothing of Sprout reaches the domain transaction.
fn skip_joinsplits(reader: &mut Reader<'_>, groth: bool) -> Result<(), DecodeError> {
    let count = reader.compact_size()?;
    let proof = if groth { GROTH_PROOF } else { PHGR_PROOF };
    let size = count
        .checked_mul(JOINSPLIT_WITHOUT_PROOF + proof)
        .ok_or(DecodeError::CompactSizeTooLarge(u64::MAX))?;
    reader.skip(size)?;
    if count > 0 {
        reader.skip(32 + SIGNATURE)?;
    }
    Ok(())
}
