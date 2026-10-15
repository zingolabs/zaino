//! [`TxBuilder`] → consensus bytes, written by librustzcash
//!
//! - A test states values, spends and note tags
//! - This file fills in the rest
//! - Each shielded description laid out field by field, then parsed by its crate's `from_bytes`
//!   (the reader's canonical checks: no bytes the decoder would refuse)
//! - Points, proofs, signatures = deterministic filler (the reader never decompresses them)

use nonempty::NonEmpty;
use orchard::bundle::{Authorized as OrchardAuth, Flags};
use orchard::primitives::redpallas::Signature as PallasSignature;
use orchard::{ActionBytes, Anchor, Proof, ValuePool};
use sapling_crypto::bundle::{
    Authorized as SaplingAuth, BundleBytes as SaplingBytes, OutputDescriptionBytes,
    SpendDescriptionBytes,
};
use zaino_primitives::sha256d;
use zaino_primitives::types::{NoteCommitment, Nullifier};
use zcash_primitives::transaction::components::orchard::bundle_version_for_branch;
use zcash_primitives::transaction::components::sprout::{
    Bundle as SproutBundle, JsDescription, SproutProof, NOTE_CIPHERTEXT_SIZE,
};
use zcash_primitives::transaction::{CompressedTransactionData, TxVersion};
use zcash_protocol::consensus::{BlockHeight, BranchId, NetworkUpgrade, Parameters};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::value::{ZatBalance, Zatoshis};
use zcash_script::script::Code;
use zcash_transparent::address::Script;
use zcash_transparent::bundle::{Authorized, Bundle as TransparentBundle, OutPoint, TxIn, TxOut};

use super::build::TxBuilder;
use super::Refused;

type OrchardBytes = orchard::BundleBytes<OrchardAuth, ZatBalance>;

/// Nullifier for note tag `tag`: little-endian (canonical in every pool's field)
pub fn nullifier(tag: u32) -> Nullifier {
    Nullifier(le_bytes(tag))
}

/// Note commitment (`cmu` / `cmx`) for leaf `leaf`: little-endian (canonical in every pool's field)
pub fn commitment(leaf: u32) -> NoteCommitment {
    NoteCommitment(le_bytes(leaf))
}

/// Where one transaction is minted: `nonce` = the chain's mint counter (makes every tx's bytes
/// unique: the coinbase's extranonce, every other one's lock time)
pub(super) struct At<'a> {
    pub(super) height: BlockHeight,
    pub(super) upgrades: &'a LocalNetwork,
    pub(super) nonce: u32,
}

/// One transaction's bytes (`prevouts` = its spends, already resolved to outpoints)
pub(super) fn transaction(
    tx: &TxBuilder,
    prevouts: Vec<OutPoint>,
    coinbase: bool,
    at: At,
) -> Vec<u8> {
    let version = version(tx, &at);
    let branch = BranchId::for_height(at.upgrades, at.height);

    let transparent = match coinbase {
        true => coinbase_transparent(tx, &at),
        false => transparent(tx, prevouts),
    };
    let sprout = sprout(tx.sprout_balance);
    let sapling = sapling(tx, &at);
    let orchard =
        actions(&tx.orchard_actions, tx.orchard_balance, ValuePool::Orchard, branch, coinbase, &at);
    let ironwood = actions(
        &tx.ironwood_actions,
        tx.ironwood_balance,
        ValuePool::Ironwood,
        branch,
        coinbase,
        &at,
    );

    // lock time: always final (every input's sequence = u32::MAX)
    let lock_time = if coinbase { 0 } else { at.nonce };
    // ZIP 203: an NU5+ coinbase expires at its height (its txid's only height commitment: v5
    // txids skip scriptSig)
    let nu5 = at.upgrades.is_nu_active(NetworkUpgrade::Nu5, at.height);
    let expiry = if coinbase && nu5 { at.height } else { BlockHeight::from(0) };
    let data = match version {
        TxVersion::V6 => CompressedTransactionData::from_parts_v6(
            branch,
            lock_time,
            expiry,
            transparent,
            sapling,
            orchard,
            ironwood,
        ),
        _ => CompressedTransactionData::from_parts(
            version,
            branch,
            lock_time,
            expiry,
            transparent,
            sprout,
            sapling,
            orchard,
        ),
    };

    let mut raw = Vec::new();
    let tx = data.freeze().expect("a minted transaction hashes");
    tx.write(&mut raw).expect("a Vec takes every write");
    raw
}

/// The format a node at `height` writes for this content
///
/// - Ironwood → v6
/// - Sprout → v4 (v5 dropped JoinSplits)
/// - else NU5 → v5, Sapling → v4, Overwinter → v3, before → v1
fn version(tx: &TxBuilder, at: &At) -> TxVersion {
    let active = |upgrade| at.upgrades.is_nu_active(upgrade, at.height);
    let require = |upgrade, used: bool| {
        assert!(!used || active(upgrade), "{}", Refused::Inactive(upgrade));
    };
    let sapling = !tx.sapling_spends.is_empty() || !tx.sapling_outputs.is_empty();
    let orchard = !tx.orchard_actions.is_empty();
    let ironwood = !tx.ironwood_actions.is_empty();
    let sprout = tx.sprout_balance != 0;
    require(NetworkUpgrade::Sapling, sapling || sprout);
    require(NetworkUpgrade::Nu5, orchard);
    require(NetworkUpgrade::Nu6_3, ironwood);
    assert!(
        !(sprout && (orchard || ironwood)),
        "sprout_balance() beside Orchard / Ironwood: no format carries both"
    );

    match () {
        _ if ironwood => TxVersion::V6,
        _ if sprout => TxVersion::V4,
        _ if active(NetworkUpgrade::Nu5) => TxVersion::V5,
        _ if active(NetworkUpgrade::Sapling) => TxVersion::V4,
        _ if active(NetworkUpgrade::Overwinter) => TxVersion::V3,
        _ => TxVersion::Sprout(1),
    }
}

/// Null prevout, script = BIP 34 height then the extranonce (a coinbase script holds ≥ 2 bytes)
fn coinbase_transparent(tx: &TxBuilder, at: &At) -> Option<TransparentBundle<Authorized>> {
    assert!(tx.spends.is_empty(), "{}", Refused::CoinbaseSpends);
    let script = [bip34(u32::from(at.height)), push(&at.nonce.to_le_bytes())].concat();
    let input = TxIn::from_parts(OutPoint::NULL, Script(Code(script)), u32::MAX);
    Some(TransparentBundle { vin: vec![input], vout: outputs(tx), authorization: Authorized })
}

/// Empty script sigs (nothing here verifies a signature)
fn transparent(tx: &TxBuilder, prevouts: Vec<OutPoint>) -> Option<TransparentBundle<Authorized>> {
    let vin: Vec<TxIn<Authorized>> = prevouts
        .into_iter()
        .map(|prevout| TxIn::from_parts(prevout, Script::default(), u32::MAX))
        .collect();
    let vout = outputs(tx);
    if vin.is_empty() && vout.is_empty() {
        return None;
    }
    Some(TransparentBundle { vin, vout, authorization: Authorized })
}

fn outputs(tx: &TxBuilder) -> Vec<TxOut> {
    let output = |(script, zats): &(Script, u64)| {
        let value = Zatoshis::from_u64(*zats).unwrap_or_else(|_| panic!("{}", Refused::PastSupply));
        TxOut::new(value, script.clone())
    };
    tx.pays.iter().map(output).collect()
}

/// One JoinSplit: `vpub_new − vpub_old` = the balance (+ = out of Sprout)
fn sprout(balance: i64) -> Option<SproutBundle> {
    if balance == 0 {
        return None;
    }
    let zats =
        |zats: i64| Zatoshis::from_u64(zats.unsigned_abs()).expect("a balance within the supply");
    let (vpub_old, vpub_new) = match balance < 0 {
        true => (zats(balance), Zatoshis::ZERO),
        false => (Zatoshis::ZERO, zats(balance)),
    };
    let joinsplit = JsDescription::from_parts(
        vpub_old,
        vpub_new,
        [0; 32],            // anchor
        [[1; 32], [2; 32]], // nullifiers
        [[3; 32], [4; 32]], // commitments
        [5; 32],            // ephemeral key
        [6; 32],            // random seed
        [[7; 32], [8; 32]], // macs
        SproutProof::Groth([9; 192]),
        [[10; NOTE_CIPHERTEXT_SIZE]; 2],
    );
    Some(SproutBundle {
        joinsplits: vec![joinsplit],
        joinsplit_pubkey: [11; 32],
        joinsplit_sig: [12; 64],
    })
}

fn sapling(tx: &TxBuilder, at: &At) -> Option<SaplingBytes<SaplingAuth, ZatBalance>> {
    let spends = tx.sapling_spends.iter().enumerate().map(|(index, tag)| {
        let field = |name: &str| filler::<32>(at, "sapling spend", name, index);
        let bytes = [
            field("cv"),
            [0; 32], // anchor
            le_bytes(*tag),
            field("rk"),
        ]
        .concat();
        let bytes = [
            bytes,
            filler::<192>(at, "sapling spend", "zkproof", index).to_vec(),
            filler::<64>(at, "sapling spend", "sig", index).to_vec(),
        ]
        .concat();
        let bytes = bytes.try_into().expect("cv ‖ anchor ‖ nf ‖ rk ‖ zkproof ‖ sig = 384 bytes");
        SpendDescriptionBytes::from_bytes(&bytes).expect("a zero anchor is canonical")
    });
    let outputs = tx.sapling_outputs.iter().enumerate().map(|(index, leaf)| {
        let bytes = [
            filler::<32>(at, "sapling output", "cv", index).to_vec(),
            le_bytes(*leaf).to_vec(),
            filler::<32>(at, "sapling output", "epk", index).to_vec(),
            filler::<580>(at, "sapling output", "enc", index).to_vec(),
            filler::<80>(at, "sapling output", "out", index).to_vec(),
            filler::<192>(at, "sapling output", "zkproof", index).to_vec(),
        ]
        .concat();
        let bytes = bytes.try_into().expect("cv ‖ cmu ‖ epk ‖ enc ‖ out ‖ zkproof = 948 bytes");
        OutputDescriptionBytes::from_bytes(&bytes).expect("a leaf tag is a canonical cmu")
    });

    let binding_sig = redjubjub::Signature::from(filler::<64>(at, "sapling", "binding sig", 0));
    let bundle = SaplingBytes::from_parts(
        spends.collect(),
        outputs.collect(),
        balance(tx.sapling_balance),
        SaplingAuth { binding_sig },
    );
    assert!(bundle.is_some() || tx.sapling_balance == 0, "value_balance(Sapling) with no Sapling spend or output (an empty bundle carries no balance)");
    bundle
}

/// The Orchard or Ironwood bundle: one shape, the bundle version the branch reads it with
fn actions(
    actions: &[(u32, u32)],
    zats: i64,
    pool: ValuePool,
    branch: BranchId,
    coinbase: bool,
    at: &At,
) -> Option<OrchardBytes> {
    let Some(actions) = NonEmpty::from_slice(actions) else {
        assert_eq!(
            zats, 0,
            "value_balance({pool:?}) with no action (an empty bundle carries no balance)"
        );
        return None;
    };
    let label = match pool {
        ValuePool::Orchard => "orchard",
        ValuePool::Ironwood => "ironwood",
    };
    let action = |index: usize, (tag, leaf): (u32, u32)| {
        let field = |name: &str| filler::<32>(at, label, name, index).to_vec();
        let bytes = [
            field("cv_net"),
            le_bytes(tag).to_vec(),
            field("rk"),
            le_bytes(leaf).to_vec(),
            field("epk"),
            filler::<580>(at, label, "enc", index).to_vec(),
            filler::<80>(at, label, "out", index).to_vec(),
        ]
        .concat();
        let bytes = bytes.try_into().expect("cv_net ‖ nf ‖ rk ‖ cmx ‖ epk ‖ enc ‖ out = 820 bytes");
        let sig = PallasSignature::from(filler::<64>(at, label, "spend auth sig", index));
        ActionBytes::from_bytes(&bytes)
            .expect("nullifier and leaf tags are canonical")
            .with_authorization(sig)
    };
    let actions =
        NonEmpty::collect(actions.into_iter().enumerate().map(|(index, tags)| action(index, tags)))
            .expect("a NonEmpty maps to a NonEmpty");

    let version =
        bundle_version_for_branch(branch, pool).expect("pool active (checked by version())");
    // spends + outputs (a coinbase only creates notes)
    let flags = Flags::from_byte(if coinbase { 0b10 } else { 0b11 }, version)
        .expect("bit 2 clear: every version reads it");
    let proof = Proof::new(vec![0x5a; Proof::expected_proof_size(actions.len())]);
    let binding_sig = PallasSignature::from(filler::<64>(at, label, "binding sig", 0));
    let anchor = Option::from(Anchor::from_bytes([0; 32])).expect("a zero anchor is canonical");
    let bundle = OrchardBytes::try_from_parts(
        actions,
        flags,
        balance(zats),
        anchor,
        OrchardAuth::from_parts(proof, binding_sig),
        version,
    );
    Some(bundle.expect("canonical proof size, representable flags"))
}

fn balance(zats: i64) -> ZatBalance {
    ZatBalance::from_i64(zats).unwrap_or_else(|_| panic!("{}", Refused::PastSupply))
}

fn le_bytes(tag: u32) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..4].copy_from_slice(&tag.to_le_bytes());
    bytes
}

/// `N` bytes no other field of any minted transaction shares: SHA-256d over (nonce, what, index)
fn filler<const N: usize>(at: &At, bundle: &str, field: &str, index: usize) -> [u8; N] {
    let seed =
        [&at.nonce.to_le_bytes()[..], bundle.as_bytes(), field.as_bytes(), &index.to_le_bytes()]
            .concat();
    let blocks = (0u32..).map(|counter| sha256d(&[&seed[..], &counter.to_le_bytes()].concat()));
    let bytes: Vec<u8> = blocks.flatten().take(N).collect();
    bytes.try_into().expect("took N bytes")
}

/// BIP 34 height push: `OP_0`, `OP_1`..`OP_16`, else minimal little-endian (`CScriptNum`)
pub(super) fn bip34(height: u32) -> Vec<u8> {
    match height {
        0 => vec![0x00],
        1..=16 => vec![0x50 + height as u8],
        _ => {
            let mut number: Vec<u8> = height.to_le_bytes().into_iter().collect();
            while number.last() == Some(&0) {
                number.pop();
            }
            // top bit = sign: one more byte keeps it positive
            if number.last().is_some_and(|byte| byte & 0x80 != 0) {
                number.push(0);
            }
            push(&number)
        }
    }
}

fn push(bytes: &[u8]) -> Vec<u8> {
    let len = u8::try_from(bytes.len()).expect("a short push");
    [&[len][..], bytes].concat()
}
