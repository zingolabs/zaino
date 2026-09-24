//! Domain -> wire conversion, owned by the adapter.
//!
//! Conversion lives here, not on the domain types, so the domain crate never
//! depends on `zaino-proto`. A local extension trait keeps the `to_wire()`
//! naming and direction while respecting the orphan rule (foreign domain type,
//! local trait).

use zaino_core::{
    AddressBalance, BlockId, ChainMetadata, CompactBlock, CompactCiphertext, Nullifier,
    OrchardAction, PreIndexCompactTx, RawTransaction, SaplingOutput, SubtreeRoot,
    TransactionLocation, TransparentInput, TransparentOutput, Treestate, Utxo,
};
use zaino_proto::proto::compact_formats as cf;
use zaino_proto::proto::service as proto;

pub(crate) trait ToWire {
    type Wire;
    fn to_wire(self) -> Self::Wire;
}

impl ToWire for BlockId {
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

impl ToWire for Treestate {
    type Wire = proto::TreeState;

    fn to_wire(self) -> proto::TreeState {
        proto::TreeState {
            // The handler is not parameterised by the network (see
            // `get_lightd_info`), so `network` rides out empty best-effort — a
            // wallet reads the height and the serialized trees, not this field.
            network: String::new(),
            height: u64::from(self.height),
            // Display (big-endian) order, as `z_gettreestate` reports the hash.
            hash: self.block_hash.to_string(),
            // `BlockTime` is a Unix-epoch `u32`; the wire field is the same.
            time: self.time,
            // Each pool's serialized tree rides out as lowercase hex; an inactive
            // pool at this block is signalled by an empty string, never a
            // serialized empty tree (which would claim the pool is active).
            sapling_tree: self
                .sapling
                .map(|pool| hex_bytes(&pool.final_state))
                .unwrap_or_default(),
            orchard_tree: self
                .orchard
                .map(|pool| hex_bytes(&pool.final_state))
                .unwrap_or_default(),
            ironwood_tree: self
                .ironwood
                .map(|pool| hex_bytes(&pool.final_state))
                .unwrap_or_default(),
        }
    }
}

impl ToWire for SubtreeRoot {
    type Wire = proto::SubtreeRoot;

    fn to_wire(self) -> proto::SubtreeRoot {
        proto::SubtreeRoot {
            root_hash: <[u8; 32]>::from(self.root).to_vec(),
            // The domain subtree root carries only the root and the completing
            // height (as `z_getsubtreesbyindex` reports), not the completing
            // block hash, so that field rides out empty.
            completing_block_hash: Vec::new(),
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

/// One compact transaction. `index` is its position within the block. The
/// transparent, shielded, and ironwood components each map to their wire shape;
/// `fee` is left unset (0) — a stateless index cannot compute it.
fn compact_tx_to_wire(index: u64, tx: PreIndexCompactTx) -> cf::CompactTx {
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
    use super::{hex_bytes, ToWire};
    use zaino_core::{BlockHash, Height, SubtreeRoot, Treestate};
    // `PoolTreestate`/`TreeRoot` are domain component types the `zaino-core`
    // facade does not re-export; the production conversions never name them, only
    // these tests construct them, so they come straight from primitives here.
    use zaino_primitives::types::{PoolTreestate, TreeRoot};

    /// A treestate maps field-for-field to the wire shape: height/time straight
    /// through, the hash in display (big-endian) order, an active pool's tree as
    /// lowercase hex, and an inactive pool as the empty string (never a
    /// serialized empty tree).
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

        let wire = treestate.to_wire();
        assert_eq!(wire.height, 2_800_000u64);
        assert_eq!(wire.time, 1_700_000_000u32);
        // Display order: the all-0xAB hash renders the same forwards, but the
        // length and casing are the contract wallets read.
        assert_eq!(wire.hash, "ab".repeat(32));
        assert_eq!(wire.sapling_tree, "deadbeef");
        assert_eq!(wire.orchard_tree, "");
        assert_eq!(wire.ironwood_tree, "");
        assert_eq!(wire.network, "");
    }

    /// A subtree root maps its root bytes and completing height; the completing
    /// block hash the domain does not carry rides out empty.
    #[test]
    fn subtree_root_maps_to_wire() {
        let root = SubtreeRoot {
            root: TreeRoot::from([0x11u8; 32]),
            end_height: Height::try_from(1_000_000).expect("valid height"),
        };

        let wire = root.to_wire();
        assert_eq!(wire.root_hash, vec![0x11u8; 32]);
        assert_eq!(wire.completing_block_height, 1_000_000u64);
        assert!(wire.completing_block_hash.is_empty());
    }

    #[test]
    fn hex_bytes_is_lowercase_and_padded() {
        assert_eq!(hex_bytes(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex_bytes(&[]), "");
    }

    /// An address balance maps its current balance to the signed wire value.
    #[test]
    fn address_balance_maps_to_wire() {
        use zaino_core::AddressBalance;
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
        use zaino_core::{Height, Script, TransactionId, TransparentAddress, Utxo};
        use zaino_primitives::types::Zatoshis;
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
        use zaino_core::{Height, RawTransaction, TransactionLocation};
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
}
