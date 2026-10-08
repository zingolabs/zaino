//! Domain -> wire conversion, owned by the adapter.
//!
//! Conversion lives here, not on the domain types, so the domain crate never
//! depends on `zaino-proto`. A local extension trait keeps the `to_wire()`
//! naming and direction while respecting the orphan rule (foreign domain type,
//! local trait).

use zaino_primitives::types::{
    AddressBalance, BlockRef, ChainMetadata, CompactBlock, CompactCiphertext, Nullifier,
    OrchardAction, PreIndexCompactTx, RawTransaction, SaplingOutput, SubtreeRoot,
    TransactionLocation, TransparentInput, TransparentOutput, Treestate, Utxo,
};
use zaino_proto::proto::compact_formats as cf;
use zaino_proto::proto::service as proto;
use zcash_protocol::consensus::Network;

pub(crate) trait ToWire {
    type Wire;
    fn to_wire(self) -> Self::Wire;
}

impl ToWire for BlockRef {
    type Wire = proto::BlockId;

    fn to_wire(self) -> proto::BlockId {
        proto::BlockId {
            height: u64::from(self.height),
            hash: <[u8; 32]>::from(self.hash).to_vec(),
        }
    }
}

impl ToWire for CompactBlock {
    type Wire = cf::CompactBlock;

    fn to_wire(self) -> cf::CompactBlock {
        cf::CompactBlock {
            height: u64::from(self.height),
            hash: <[u8; 32]>::from(self.hash).to_vec(),
            prev_hash: <[u8; 32]>::from(self.prev_hash).to_vec(),
            // `BlockTime` is a Unix-epoch `u32`; the wire field is the same.
            time: self.time,
            // The full 80-byte header is optional in the compact format and the
            // index does not retain it, so it rides out empty.
            header: Vec::new(),
            vtx: self
                .transactions
                .into_iter()
                .enumerate()
                .map(|(index, tx)| compact_tx_to_wire(index_as_u64(index), tx))
                .collect(),
            chain_metadata: Some(self.chain_metadata.to_wire()),
        }
    }
}

impl ToWire for ChainMetadata {
    type Wire = cf::ChainMetadata;

    fn to_wire(self) -> cf::ChainMetadata {
        cf::ChainMetadata {
            sapling_commitment_tree_size: u32::from(self.sapling_tree_size),
            orchard_commitment_tree_size: u32::from(self.orchard_tree_size),
            ironwood_commitment_tree_size: u32::from(self.ironwood_tree_size),
        }
    }
}

/// The lightwalletd/BIP70 network name, matching zebra's `bip70_network_name`
/// (the same value lightwalletd's `GetLightdInfo.chainName` carries): mainnet is
/// "main", every test network — public testnet and regtest alike — is "test".
/// `zcash_protocol`'s `Network` collapses regtest into `TestNetwork` upstream, so
/// regtest reaches here as "test", which is exactly what zebra renders.
pub(crate) fn bip70_network_name(network: Network) -> &'static str {
    match network {
        Network::MainNetwork => "main",
        Network::TestNetwork => "test",
    }
}

/// A treestate as the wire `TreeState`, rendered for the network the adapter
/// serves. Not a `ToWire` impl: the conversion needs the served network (the
/// domain `Treestate` does not carry it), which the trait's parameterless
/// `to_wire` cannot supply.
pub(crate) fn treestate_to_wire(treestate: Treestate, network: Network) -> proto::TreeState {
    proto::TreeState {
        // The BIP70 network name, as zebra and lightwalletd render it; the
        // handler is parameterised by the network it serves.
        network: bip70_network_name(network).to_string(),
        height: u64::from(treestate.height),
        // Display (big-endian) order, as `z_gettreestate` reports the hash.
        hash: treestate.block_hash.to_string(),
        // `BlockTime` is a Unix-epoch `u32`; the wire field is the same.
        time: treestate.time,
        // Each pool's serialized tree rides out as lowercase hex; an inactive
        // pool at this block is signalled by an empty string, never a
        // serialized empty tree (which would claim the pool is active).
        sapling_tree: treestate
            .sapling
            .map(|pool| hex_bytes(&pool.final_state))
            .unwrap_or_default(),
        orchard_tree: treestate
            .orchard
            .map(|pool| hex_bytes(&pool.final_state))
            .unwrap_or_default(),
        ironwood_tree: treestate
            .ironwood
            .map(|pool| hex_bytes(&pool.final_state))
            .unwrap_or_default(),
    }
}

impl ToWire for SubtreeRoot {
    type Wire = proto::SubtreeRoot;

    fn to_wire(self) -> proto::SubtreeRoot {
        // The root is a commitment-tree value, not an identifier, so it rides out
        // in internal (unreversed) order. The completing block hash is a block
        // identifier, so — like every hash on this interface, and matching
        // lightwalletd — it is reversed to display (big-endian) order.
        let mut completing_block_hash = <[u8; 32]>::from(self.completing_block_hash);
        completing_block_hash.reverse();
        proto::SubtreeRoot {
            root_hash: <[u8; 32]>::from(self.root).to_vec(),
            completing_block_hash: completing_block_hash.to_vec(),
            completing_block_height: u64::from(self.end_height),
        }
    }
}

impl ToWire for AddressBalance {
    type Wire = proto::Balance;

    fn to_wire(self) -> proto::Balance {
        // The wire carries only the current balance (not lifetime receipts).
        proto::Balance {
            value_zat: zat_to_i64(u64::from(self.balance)),
        }
    }
}

impl ToWire for Utxo {
    type Wire = proto::GetAddressUtxosReply;

    fn to_wire(self) -> proto::GetAddressUtxosReply {
        proto::GetAddressUtxosReply {
            address: self.address.as_str().to_string(),
            txid: <[u8; 32]>::from(self.txid).to_vec(),
            // `OutputIndex` is a `u32`; the wire field is `i32`. A real output
            // index is tiny, so the saturating fallback is unreachable — it only
            // keeps the conversion total without an `as` cast.
            index: i32::try_from(self.output_index).unwrap_or(i32::MAX),
            script: Vec::<u8>::from(self.script),
            value_zat: zat_to_i64(u64::from(self.satoshis)),
            height: u64::from(self.height),
        }
    }
}

impl ToWire for RawTransaction {
    type Wire = proto::RawTransaction;

    fn to_wire(self) -> proto::RawTransaction {
        proto::RawTransaction {
            data: self.data.into(),
            // The lightwalletd `height` field is overloaded: 0 for a mempool tx,
            // `u64::MAX` for one mined on a non-best fork, else the mined height.
            height: match self.location {
                TransactionLocation::BestChain(height) => u64::from(height),
                TransactionLocation::NonBestChain => u64::MAX,
                TransactionLocation::Mempool => 0,
            },
        }
    }
}

/// A supply-bounded zatoshi amount as the wire's signed `value_zat`. `Zatoshis`
/// never exceeds the money supply (far below `i64::MAX`), so the saturating
/// fallback is unreachable — it only keeps the conversion total without an `as`
/// cast. `pub(crate)` so the balance handler, which sums per-address balances
/// into one wire `Balance`, shares the one zat -> wire rule.
pub(crate) fn zat_to_i64(zatoshis: u64) -> i64 {
    i64::try_from(zatoshis).unwrap_or(i64::MAX)
}

/// A transaction's position within its block. `usize -> u64` is lossless on
/// every supported platform (a block holds at most `usize` transactions).
fn index_as_u64(index: usize) -> u64 {
    u64::try_from(index).expect("a block's tx count fits u64")
}

/// One compact transaction. `index` is its position within the block (or, for a
/// mempool listing, within the stream). The transparent, shielded, and ironwood
/// components each map to their wire shape; `fee` is left unset (0) — a stateless
/// index cannot compute it.
pub(crate) fn compact_tx_to_wire(index: u64, tx: PreIndexCompactTx) -> cf::CompactTx {
    cf::CompactTx {
        index,
        txid: <[u8; 32]>::from(tx.txid).to_vec(),
        fee: 0,
        spends: tx.sapling_nullifiers.into_iter().map(spend).collect(),
        outputs: tx.sapling_outputs.into_iter().map(sapling_output).collect(),
        actions: tx.orchard_actions.into_iter().map(orchard_action).collect(),
        ironwood_actions: tx
            .ironwood_actions
            .into_iter()
            .map(orchard_action)
            .collect(),
        vin: tx.transparent_inputs.into_iter().map(txin).collect(),
        vout: tx.transparent_outputs.into_iter().map(txout).collect(),
    }
}

fn spend(nullifier: Nullifier) -> cf::CompactSaplingSpend {
    cf::CompactSaplingSpend {
        nf: <[u8; 32]>::from(nullifier).to_vec(),
    }
}

fn sapling_output(output: SaplingOutput) -> cf::CompactSaplingOutput {
    cf::CompactSaplingOutput {
        cmu: <[u8; 32]>::from(output.cmu).to_vec(),
        ephemeral_key: <[u8; 32]>::from(output.ephemeral_key).to_vec(),
        ciphertext: <[u8; CompactCiphertext::LENGTH]>::from(output.enc_ciphertext).to_vec(),
    }
}

fn orchard_action(action: OrchardAction) -> cf::CompactOrchardAction {
    cf::CompactOrchardAction {
        nullifier: <[u8; 32]>::from(action.nullifier).to_vec(),
        cmx: <[u8; 32]>::from(action.cmx).to_vec(),
        ephemeral_key: <[u8; 32]>::from(action.ephemeral_key).to_vec(),
        ciphertext: <[u8; CompactCiphertext::LENGTH]>::from(action.enc_ciphertext).to_vec(),
    }
}

fn txin(input: TransparentInput) -> cf::CompactTxIn {
    cf::CompactTxIn {
        prevout_txid: <[u8; 32]>::from(input.prev_txid).to_vec(),
        prevout_index: input.prev_index,
    }
}

fn txout(output: TransparentOutput) -> cf::TxOut {
    cf::TxOut {
        value: u64::from(output.value),
        script_pub_key: Vec::<u8>::from(output.script),
    }
}

/// Lowercase hex of an arbitrary byte slice, so a domain blob (e.g. a serialized
/// commitment tree) can ride out on a wire string field without a hex dependency.
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex of a 32-byte id, so a domain id can ride out on a wire string
/// field without a hex dependency.
pub(crate) fn to_hex(bytes: [u8; 32]) -> String {
    hex_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::{bip70_network_name, hex_bytes, treestate_to_wire, ToWire};
    use zaino_primitives::types::{BlockHash, Height, SubtreeRoot, Treestate};
    use zcash_protocol::consensus::Network;
    // `PoolTreestate`/`TreeRoot` are domain component types the `zaino-primitives`
    // facade does not re-export; the production conversions never name them, only
    // these tests construct them, so they come straight from primitives here.
    use zaino_primitives::types::{PoolTreestate, TreeRoot};

    /// A treestate maps field-for-field to the wire shape: height/time straight
    /// through, the hash in display (big-endian) order, an active pool's tree as
    /// lowercase hex, an inactive pool as the empty string (never a serialized
    /// empty tree), and the network rendered as the served chain's BIP70 name.
    #[test]
    fn treestate_maps_to_wire() {
        let treestate = Treestate {
            block_hash: BlockHash::from([0xABu8; 32]),
            height: Height::try_from(2_800_000).expect("valid height"),
            time: 1_700_000_000,
            sapling: Some(PoolTreestate {
                final_root: None,
                final_state: vec![0xde, 0xad, 0xbe, 0xef],
            }),
            orchard: None,
            ironwood: None,
        };

        let wire = treestate_to_wire(treestate, Network::MainNetwork);
        assert_eq!(wire.height, 2_800_000u64);
        assert_eq!(wire.time, 1_700_000_000u32);
        // Display order: the all-0xAB hash renders the same forwards, but the
        // length and casing are the contract wallets read.
        assert_eq!(wire.hash, "ab".repeat(32));
        assert_eq!(wire.sapling_tree, "deadbeef");
        assert_eq!(wire.orchard_tree, "");
        assert_eq!(wire.ironwood_tree, "");
        assert_eq!(wire.network, "main");
    }

    /// The network field tracks the served chain's BIP70 name: "main" on
    /// mainnet, "test" on every test network (public testnet and regtest, which
    /// `zcash_protocol`'s `Network` collapses into `TestNetwork`). This is the
    /// field an in-cluster byte-compare against zebra flagged as the sole
    /// difference, so it is pinned per network.
    #[test]
    fn treestate_network_tracks_served_chain() {
        let at = |network| {
            let treestate = Treestate {
                block_hash: BlockHash::from([0x00u8; 32]),
                height: Height::try_from(1).expect("valid height"),
                time: 0,
                sapling: None,
                orchard: None,
                ironwood: None,
            };
            treestate_to_wire(treestate, network).network
        };
        assert_eq!(at(Network::MainNetwork), "main");
        assert_eq!(at(Network::TestNetwork), "test");
        assert_eq!(bip70_network_name(Network::MainNetwork), "main");
        assert_eq!(bip70_network_name(Network::TestNetwork), "test");
    }

    /// A subtree root maps its root bytes (internal order), its completing
    /// height, and the completing block hash reversed to display order.
    #[test]
    fn subtree_root_maps_to_wire() {
        // An asymmetric hash so a missing or doubled reversal is visible: internal
        // order counts up, display order counts down.
        let mut internal = [0u8; 32];
        for (i, byte) in internal.iter_mut().enumerate() {
            *byte = u8::try_from(i).expect("index < 32");
        }
        let mut display = internal;
        display.reverse();

        let root = SubtreeRoot {
            root: TreeRoot::from([0x11u8; 32]),
            completing_block_hash: BlockHash::from(internal),
            end_height: Height::try_from(1_000_000).expect("valid height"),
        };

        let wire = root.to_wire();
        // The root rides out in its natural (internal) order, not reversed.
        assert_eq!(wire.root_hash, vec![0x11u8; 32]);
        assert_eq!(wire.completing_block_height, 1_000_000u64);
        assert_eq!(
            wire.completing_block_hash,
            display.to_vec(),
            "the completing block hash is reversed to display order"
        );
    }

    #[test]
    fn hex_bytes_is_lowercase_and_padded() {
        assert_eq!(hex_bytes(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex_bytes(&[]), "");
    }

    /// An address balance maps its current balance to the signed wire value.
    #[test]
    fn address_balance_maps_to_wire() {
        use zaino_primitives::types::AddressBalance;
        use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum};
        let balance = AddressBalance {
            balance: Zatoshis::new(123_456).expect("valid amount"),
            received: ZatoshisFlowSum::from_summed(999),
        };
        assert_eq!(balance.to_wire().value_zat, 123_456i64);
    }

    /// A UTXO maps field-for-field: address string, txid bytes, index (u32 ->
    /// i32), script bytes, value, and height.
    #[test]
    fn utxo_maps_to_wire() {
        use zaino_primitives::types::Zatoshis;
        use zaino_primitives::types::{Height, Script, TransactionId, TransparentAddress, Utxo};
        let utxo = Utxo {
            address: TransparentAddress::new("t1example".to_string()),
            txid: TransactionId::from([0x22u8; 32]),
            output_index: 3,
            script: Script::new(vec![0x76, 0xa9]),
            satoshis: Zatoshis::new(50_000).expect("valid amount"),
            height: Height::try_from(2_000_000).expect("valid height"),
        };

        let wire = utxo.to_wire();
        assert_eq!(wire.address, "t1example");
        assert_eq!(wire.txid, vec![0x22u8; 32]);
        assert_eq!(wire.index, 3i32);
        assert_eq!(wire.script, vec![0x76, 0xa9]);
        assert_eq!(wire.value_zat, 50_000i64);
        assert_eq!(wire.height, 2_000_000u64);
    }

    /// A raw transaction maps its bytes, and its location to the overloaded
    /// lightwalletd `height`: the mined height on the best chain, `u64::MAX` on a
    /// non-best fork, and `0` in the mempool.
    #[test]
    fn raw_transaction_height_encodes_location() {
        use zaino_primitives::types::{Height, RawTransaction, TransactionLocation};
        let at = |loc| {
            RawTransaction {
                data: vec![0xde, 0xad],
                location: loc,
            }
            .to_wire()
        };
        let mined = at(TransactionLocation::BestChain(
            Height::try_from(1_234_567).expect("valid height"),
        ));
        assert_eq!(mined.data.to_vec(), vec![0xde, 0xad]);
        assert_eq!(mined.height, 1_234_567u64);
        assert_eq!(at(TransactionLocation::NonBestChain).height, u64::MAX);
        assert_eq!(at(TransactionLocation::Mempool).height, 0u64);
    }

    /// The `z_gettreestate` fixtures captured from zebra 6.4.2 (mainnet). One
    /// source of truth: the same file `zaino-indexes` tests read.
    const TREESTATE_FIXTURE: &str =
        include_str!("../../zaino-indexes/tests/fixtures/treestate/zebra-mainnet.json");

    /// Decode a hex string into exactly 32 bytes.
    fn bytes32(hex: &str) -> [u8; 32] {
        decode_hex(hex).try_into().expect("a 32-byte value")
    }

    /// Decode a lowercase/uppercase hex string into bytes (test-local, so the
    /// crate needs no hex dependency for this golden).
    fn decode_hex(hex: &str) -> Vec<u8> {
        fn nibble(c: u8) -> u8 {
            match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => panic!("invalid hex digit {c:#x}"),
            }
        }
        let bytes = hex.as_bytes();
        assert!(bytes.len().is_multiple_of(2), "even-length hex");
        bytes
            .chunks_exact(2)
            .map(|pair| (nibble(pair[0]) << 4) | nibble(pair[1]))
            .collect()
    }

    /// Reverse 32 bytes (display order <-> internal order).
    fn reversed(mut bytes: [u8; 32]) -> [u8; 32] {
        bytes.reverse();
        bytes
    }

    /// Golden `GetTreeState` proto rendering against the zebra mainnet fixtures.
    ///
    /// The lightwalletd `TreeState` carries each pool's serialized tree as the
    /// legacy `finalState` hex (verbatim, natural order) and the block hash in
    /// display order; it has no `finalRoot`, so no root orientation arises here. A
    /// pool with no tree at this height rides out as the empty string, and
    /// Ironwood is unscheduled on mainnet (empty).
    #[test]
    fn treestate_renders_zebra_fixture_state_and_hash() {
        let fixture: serde_json::Value =
            serde_json::from_str(TREESTATE_FIXTURE).expect("fixture parses");
        let entries = fixture["z_gettreestate"]
            .as_object()
            .expect("z_gettreestate object");

        let pool_from_fixture = |commitments: &serde_json::Value| -> Option<PoolTreestate> {
            let final_state = commitments["finalState"].as_str()?;
            Some(PoolTreestate {
                // The proto never reads the root; the index would supply one.
                final_root: None,
                final_state: decode_hex(final_state),
            })
        };

        for (height_key, response) in entries {
            let result = &response["result"];
            let hash_display = result["hash"].as_str().expect("hash");
            let height_num = result["height"].as_u64().expect("height");
            let time_num = result["time"].as_u64().expect("time");

            let treestate = Treestate {
                block_hash: BlockHash::from(reversed(bytes32(hash_display))),
                height: Height::try_from(u32::try_from(height_num).expect("height fits u32"))
                    .expect("valid height"),
                time: u32::try_from(time_num).expect("time fits u32"),
                sapling: pool_from_fixture(&result["sapling"]["commitments"]),
                orchard: pool_from_fixture(&result["orchard"]["commitments"]),
                ironwood: None,
            };

            // The fixtures are captured from zebra mainnet, so the served
            // network renders "main".
            let wire = treestate_to_wire(treestate, Network::MainNetwork);
            assert_eq!(wire.height, height_num, "height at {height_key}");
            assert_eq!(u64::from(wire.time), time_num, "time at {height_key}");
            assert_eq!(wire.network, "main", "network at {height_key}");
            assert_eq!(
                wire.hash, hash_display,
                "display-order hash at {height_key}"
            );

            let expected_tree = |pool: &str| -> String {
                result[pool]["commitments"]["finalState"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned()
            };
            assert_eq!(
                wire.sapling_tree,
                expected_tree("sapling"),
                "sapling tree at {height_key}"
            );
            assert_eq!(
                wire.orchard_tree,
                expected_tree("orchard"),
                "orchard tree at {height_key}"
            );
            assert_eq!(wire.ironwood_tree, "", "ironwood empty at {height_key}");
        }
    }

    /// Golden `GetSubtreeRoots` proto rendering: the root rides out as raw bytes
    /// in internal (unreversed) order for both pools, so `root_hash` is the
    /// fixture root's bytes verbatim, while the completing block hash is reversed
    /// to display order. The fixture carries no completing block hash for these
    /// heights, so the hash is a constructed asymmetric value and the orientation
    /// is what the test pins.
    #[test]
    fn subtree_roots_render_internal_bytes_from_fixture() {
        // Asymmetric under reversal, so a missing or doubled reversal shows up.
        let mut internal_hash = [0u8; 32];
        for (i, byte) in internal_hash.iter_mut().enumerate() {
            *byte = u8::try_from(i).expect("index < 32");
        }
        let mut display_hash = internal_hash;
        display_hash.reverse();

        let fixture: serde_json::Value =
            serde_json::from_str(TREESTATE_FIXTURE).expect("fixture parses");
        for pool_name in ["sapling", "orchard"] {
            let subtrees = fixture["z_getsubtreesbyindex"][pool_name]["result"]["subtrees"]
                .as_array()
                .expect("subtrees array");
            for subtree in subtrees {
                let root_hex = subtree["root"].as_str().expect("root hex");
                let end_height = subtree["end_height"].as_u64().expect("end_height");
                let domain = SubtreeRoot {
                    root: TreeRoot::from(bytes32(root_hex)),
                    completing_block_hash: BlockHash::from(internal_hash),
                    end_height: Height::try_from(
                        u32::try_from(end_height).expect("height fits u32"),
                    )
                    .expect("valid height"),
                };
                let wire = domain.to_wire();
                assert_eq!(
                    wire.root_hash,
                    decode_hex(root_hex),
                    "{pool_name} root verbatim"
                );
                assert_eq!(wire.completing_block_height, end_height);
                assert_eq!(
                    wire.completing_block_hash,
                    display_hash.to_vec(),
                    "{pool_name} completing block hash in display order"
                );
            }
        }
    }
}
