//! Transaction and block identifiers over the encoding.
//!
//! v1–v4 ids are the double-SHA256 of the whole transaction encoding, as is
//! the block hash of the header. v5 and v6 ids are the ZIP-244 digest tree,
//! every leaf of which is a BLAKE2b-256 over field bytes exactly as encoded —
//! which is why the walk can keep everything as bytes and still produce the
//! id a full deserialiser would.
//!
//! ```text
//! txid_v5 = H_branch( header ‖ transparent ‖ sapling ‖ orchard )
//! txid_v6 = H_branch( header ‖ transparent ‖ sapling ‖ orchard ‖ ironwood )
//! ```

use blake2b_simd::{Params, State};
use sha2::{Digest, Sha256};

use crate::transaction::{RawOrchardBundle, RawSapling, RawTransaction, Version};

/// The ZIP-244 personalisation strings.
const TX_PERSONALISATION_PREFIX: &[u8; 12] = b"ZcashTxHash_";
const HEADERS: &[u8; 16] = b"ZTxIdHeadersHash";
const TRANSPARENT: &[u8; 16] = b"ZTxIdTranspaHash";
const PREVOUTS: &[u8; 16] = b"ZTxIdPrevoutHash";
const SEQUENCE: &[u8; 16] = b"ZTxIdSequencHash";
const OUTPUTS: &[u8; 16] = b"ZTxIdOutputsHash";
const SAPLING: &[u8; 16] = b"ZTxIdSaplingHash";
const SAPLING_SPENDS: &[u8; 16] = b"ZTxIdSSpendsHash";
const SAPLING_SPENDS_COMPACT: &[u8; 16] = b"ZTxIdSSpendCHash";
const SAPLING_SPENDS_NONCOMPACT_V5: &[u8; 16] = b"ZTxIdSSpendNHash";
const SAPLING_SPENDS_NONCOMPACT_V6: &[u8; 16] = b"ZTxIdSSpendNH_v6";
const SAPLING_OUTPUTS: &[u8; 16] = b"ZTxIdSOutputHash";
const SAPLING_OUTPUTS_COMPACT: &[u8; 16] = b"ZTxIdSOutC__Hash";
const SAPLING_OUTPUTS_MEMOS: &[u8; 16] = b"ZTxIdSOutM__Hash";
const SAPLING_OUTPUTS_NONCOMPACT: &[u8; 16] = b"ZTxIdSOutN__Hash";

/// The personalisations of one Orchard-shaped bundle digest. Orchard v6 keeps
/// v5's action-level strings and changes only the bundle's; Ironwood has its
/// own throughout.
struct BundlePersonalisation {
    bundle: &'static [u8; 16],
    actions_compact: &'static [u8; 16],
    actions_memos: &'static [u8; 16],
    actions_noncompact: &'static [u8; 16],
    /// v5 commits to the anchor in the id; v6 moves it to the authorising digest.
    anchor_in_id: bool,
}

const ORCHARD_V5: BundlePersonalisation = BundlePersonalisation {
    bundle: b"ZTxIdOrchardHash",
    actions_compact: b"ZTxIdOrcActCHash",
    actions_memos: b"ZTxIdOrcActMHash",
    actions_noncompact: b"ZTxIdOrcActNHash",
    anchor_in_id: true,
};

const ORCHARD_V6: BundlePersonalisation = BundlePersonalisation {
    bundle: b"ZTxIdOrchardH_v6",
    actions_compact: b"ZTxIdOrcActCHash",
    actions_memos: b"ZTxIdOrcActMHash",
    actions_noncompact: b"ZTxIdOrcActNHash",
    anchor_in_id: false,
};

const IRONWOOD_V6: BundlePersonalisation = BundlePersonalisation {
    bundle: b"ZTxIdIronwd_H_v6",
    actions_compact: b"ZTxIdIrnActCH_v6",
    actions_memos: b"ZTxIdIrnActMH_v6",
    actions_noncompact: b"ZTxIdIrnActNH_v6",
    anchor_in_id: false,
};

/// The compact ciphertext head a light client scans, and the memo that follows it.
const COMPACT_CIPHERTEXT: usize = 52;
const MEMO_END: usize = 564;

pub(crate) fn sha256d(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(bytes)).into()
}

fn hasher(personal: &[u8; 16]) -> State {
    Params::new().hash_length(32).personal(personal).to_state()
}

fn finish(state: State) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(state.finalize().as_bytes());
    out
}

/// Append a compact-size prefix the way the encoding writes one.
fn write_compact_size(state: &mut State, n: usize) {
    let n = u64::try_from(n).expect("usize fits u64");
    if n < 253 {
        state.update(&[u8::try_from(n).expect("below 253")]);
    } else if n <= u64::from(u16::MAX) {
        state.update(&[253]);
        state.update(&u16::try_from(n).expect("fits u16").to_le_bytes());
    } else if n <= u64::from(u32::MAX) {
        state.update(&[254]);
        state.update(&u32::try_from(n).expect("fits u32").to_le_bytes());
    } else {
        state.update(&[255]);
        state.update(&n.to_le_bytes());
    }
}

pub(crate) fn transaction_id(tx: &RawTransaction<'_>) -> [u8; 32] {
    match tx.version {
        Version::Sprout(_) | Version::V3 | Version::V4 => sha256d(tx.bytes),
        Version::V5 => zip244(tx, &ORCHARD_V5, None),
        Version::V6 => zip244(tx, &ORCHARD_V6, Some(&IRONWOOD_V6)),
    }
}

fn zip244(
    tx: &RawTransaction<'_>,
    orchard: &BundlePersonalisation,
    ironwood: Option<&BundlePersonalisation>,
) -> [u8; 32] {
    let mut personal = [0u8; 16];
    personal[..12].copy_from_slice(TX_PERSONALISATION_PREFIX);
    personal[12..].copy_from_slice(&tx.consensus_branch_id.to_le_bytes());

    let mut h = hasher(&personal);
    h.update(&header_digest(tx));
    h.update(&transparent_digest(tx));
    h.update(&sapling_digest(&tx.sapling, tx.version));
    h.update(&orchard_digest(tx.orchard.as_ref(), orchard));
    if let Some(ironwood) = ironwood {
        h.update(&orchard_digest(tx.ironwood.as_ref(), ironwood));
    }
    finish(h)
}

/// T.1: the header fields, including the version group and branch ids.
fn header_digest(tx: &RawTransaction<'_>) -> [u8; 32] {
    let mut h = hasher(HEADERS);
    h.update(&tx.version.header_word().to_le_bytes());
    h.update(&tx.version.version_group_id().to_le_bytes());
    h.update(&tx.consensus_branch_id.to_le_bytes());
    h.update(&tx.lock_time.to_le_bytes());
    h.update(&tx.expiry_height.to_le_bytes());
    finish(h)
}

/// T.2: prevouts, sequences and outputs, each over every input or output;
/// empty when the transaction has neither.
fn transparent_digest(tx: &RawTransaction<'_>) -> [u8; 32] {
    let mut h = hasher(TRANSPARENT);
    if !(tx.inputs.is_empty() && tx.outputs.is_empty()) {
        let mut prevouts = hasher(PREVOUTS);
        let mut sequences = hasher(SEQUENCE);
        for input in &tx.inputs {
            prevouts.update(&input.prevout_hash);
            prevouts.update(&input.prevout_index.to_le_bytes());
            sequences.update(&input.sequence.to_le_bytes());
        }
        let mut outputs = hasher(OUTPUTS);
        for output in &tx.outputs {
            outputs.update(&output.value);
            write_compact_size(&mut outputs, output.script.len());
            outputs.update(output.script);
        }
        h.update(&finish(prevouts));
        h.update(&finish(sequences));
        h.update(&finish(outputs));
    }
    finish(h)
}

/// T.3: spends, outputs and the value balance; empty when the bundle is.
fn sapling_digest(sapling: &RawSapling<'_>, version: Version) -> [u8; 32] {
    let mut h = hasher(SAPLING);
    if !sapling.is_empty() {
        h.update(&sapling_spends_digest(sapling, version));
        h.update(&sapling_outputs_digest(sapling));
        h.update(&sapling.value_balance.to_le_bytes());
    }
    finish(h)
}

/// T.3a: nullifiers apart from the rest, so a compact block can be committed
/// to on its own. v5 writes the bundle anchor beside each spend; v6 does not.
fn sapling_spends_digest(sapling: &RawSapling<'_>, version: Version) -> [u8; 32] {
    let mut h = hasher(SAPLING_SPENDS);
    if !sapling.spends.is_empty() {
        let (noncompact_personal, anchor) = match version {
            Version::V6 => (SAPLING_SPENDS_NONCOMPACT_V6, None),
            Version::Sprout(_) | Version::V3 | Version::V4 | Version::V5 => {
                (SAPLING_SPENDS_NONCOMPACT_V5, sapling.anchor.as_ref())
            }
        };
        let mut compact = hasher(SAPLING_SPENDS_COMPACT);
        let mut noncompact = hasher(noncompact_personal);
        for spend in &sapling.spends {
            compact.update(&spend.nullifier);
            noncompact.update(&spend.cv);
            if let Some(anchor) = anchor {
                noncompact.update(anchor);
            }
            noncompact.update(&spend.rk);
        }
        h.update(&finish(compact));
        h.update(&finish(noncompact));
    }
    finish(h)
}

/// T.3b: the compact head, the memo, and the rest, apart.
fn sapling_outputs_digest(sapling: &RawSapling<'_>) -> [u8; 32] {
    let mut h = hasher(SAPLING_OUTPUTS);
    if !sapling.outputs.is_empty() {
        let mut compact = hasher(SAPLING_OUTPUTS_COMPACT);
        let mut memos = hasher(SAPLING_OUTPUTS_MEMOS);
        let mut noncompact = hasher(SAPLING_OUTPUTS_NONCOMPACT);
        for output in &sapling.outputs {
            compact.update(&output.cmu);
            compact.update(&output.ephemeral_key);
            compact.update(&output.enc_ciphertext[..COMPACT_CIPHERTEXT]);
            memos.update(&output.enc_ciphertext[COMPACT_CIPHERTEXT..MEMO_END]);
            noncompact.update(&output.cv);
            noncompact.update(&output.enc_ciphertext[MEMO_END..]);
            noncompact.update(output.out_ciphertext);
        }
        h.update(&finish(compact));
        h.update(&finish(memos));
        h.update(&finish(noncompact));
    }
    finish(h)
}

/// T.4 (and its Ironwood twin): the actions in three parts, the flags, the
/// value balance, and for v5 the anchor; an absent bundle is the bare
/// personalised hash.
fn orchard_digest(
    bundle: Option<&RawOrchardBundle<'_>>,
    personal: &BundlePersonalisation,
) -> [u8; 32] {
    let mut h = hasher(personal.bundle);
    let Some(bundle) = bundle else {
        return finish(h);
    };
    let mut compact = hasher(personal.actions_compact);
    let mut memos = hasher(personal.actions_memos);
    let mut noncompact = hasher(personal.actions_noncompact);
    for action in &bundle.actions {
        compact.update(&action.nullifier);
        compact.update(&action.cmx);
        compact.update(&action.ephemeral_key);
        compact.update(&action.enc_ciphertext[..COMPACT_CIPHERTEXT]);
        memos.update(&action.enc_ciphertext[COMPACT_CIPHERTEXT..MEMO_END]);
        noncompact.update(&action.cv);
        noncompact.update(&action.rk);
        noncompact.update(&action.enc_ciphertext[MEMO_END..]);
        noncompact.update(action.out_ciphertext);
    }
    h.update(&finish(compact));
    h.update(&finish(memos));
    h.update(&finish(noncompact));
    h.update(&[bundle.flags]);
    h.update(&bundle.value_balance.to_le_bytes());
    if personal.anchor_in_id {
        h.update(&bundle.anchor);
    }
    finish(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_size_prefixes_take_the_shortest_form() {
        let written = |n: usize| {
            let mut state = hasher(OUTPUTS);
            write_compact_size(&mut state, n);
            finish(state)
        };
        let expected = |bytes: &[u8]| {
            let mut state = hasher(OUTPUTS);
            state.update(bytes);
            finish(state)
        };
        assert_eq!(written(0), expected(&[0]));
        assert_eq!(written(252), expected(&[252]));
        assert_eq!(written(253), expected(&[253, 253, 0]));
        assert_eq!(written(0xffff), expected(&[253, 0xff, 0xff]));
        assert_eq!(written(0x1_0000), expected(&[254, 0, 0, 1, 0]));
    }
}
