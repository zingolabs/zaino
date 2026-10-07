//! Block → `Changes`: its `receives` and `spent` rows
//!
//! - projection, no lookups: a spend keyed by its outpoint (already in the block), never resolved
//!   to an address (`docs/design/index-data-structures.md` §5)
//! - parent read for its network only (the schema `Changes` is shaped by)

use zaino_persistence::{Changes, View};
use zaino_primitives::types::{Block, BlockRef};

use crate::{
    address::address_key,
    key::{encode_receive, encode_spend, ReceiveKey, ReceiveRow, Spend},
    schema, TransparentAddressReader, RECEIVES, SPENT,
};

/// `block` on top of `parent`
pub fn fold<V: View>(parent: &TransparentAddressReader<V>, block: &Block) -> Changes {
    let header = block.header();
    let tip = BlockRef { hash: header.hash, height: header.height };
    let mut changes = Changes::new(tip, &schema(parent.network()));
    let height = u32::from(header.height);
    for tx in block.transactions() {
        // coinbase inputs elided upstream (`zaino-source` decode.rs)
        for input in &tx.transparent.inputs {
            let spend = Spend { height, spender: tx.txid };
            changes.insert(SPENT, &input.encode(), &encode_spend(&spend));
        }

        for (vout, output) in (0u32..).zip(&tx.transparent.outputs) {
            let address = address_key(output.script.as_bytes());
            let key = ReceiveKey { address, height, txid: tx.txid, vout };
            let (key, value) = encode_receive(&ReceiveRow { key, value: output.value });
            changes.insert(RECEIVES, &key, &value);
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Tiered};
    use zaino_primitives::testing::linked;
    use zaino_primitives::types::{
        OutPoint, Script, Transaction, TransactionId, TransparentData, TransparentOutput, Zatoshis,
    };
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::key::AddressKey;

    /// Block 1 spends alice's block-0 receive, paying bob: its `Changes` = the spend under the
    /// outpoint + bob's receive (nothing looked up); read over the parent + it, alice's receive
    /// spent by block 1, bob's two unspent, the opaque output kept
    #[test]
    fn a_spend_folded_on_its_parent_retires_the_receive_the_parent_holds() {
        let p2pkh =
            |tag: u8| Script::new([&[0x76, 0xa9, 0x14][..], &[tag; 20], &[0x88, 0xac]].concat());
        let zat = |n: u64| Zatoshis::new(n).expect("in supply");
        let tx = |tag: u8, inputs: Vec<OutPoint>, outputs: Vec<(Script, u64)>| Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: TransparentData {
                coinbase: false,
                inputs,
                outputs: outputs
                    .into_iter()
                    .map(|(script, value)| TransparentOutput { value: zat(value), script })
                    .collect(),
            },
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        };
        let paid = OutPoint { txid: TransactionId::from([0x10; 32]), vout: 0 };
        let chain = linked(vec![
            vec![tx(
                0x10,
                vec![],
                vec![(p2pkh(0xa1), 500), (p2pkh(0xb0), 70), (Script::new(vec![0x6a]), 1)],
            )],
            vec![tx(0x20, vec![paid], vec![(p2pkh(0xb0), 490)])],
        ]);
        let (alice, bob) = (AddressKey::p2pkh([0xa1; 20]), AddressKey::p2pkh([0xb0; 20]));

        let schema = schema(NetworkType::Regtest);
        let store = DiskEngine::new(SimFs::new()).open(Path::new("/ta"), &schema);
        let mut tiered = Tiered::new(store.expect("empty store"), NonZeroUsize::MIN);
        let reader =
            |tiered: &Tiered<_>| TransparentAddressReader::new(tiered.view(), NetworkType::Regtest);
        tiered.apply(fold(&reader(&tiered), &chain[0]));
        let changes = fold(&reader(&tiered), &chain[1]);

        let spender = TransactionId::from([0x20; 32]);
        let rows = |table| {
            changes.inserts(table).map(|(k, v)| (k.to_vec(), v.to_vec())).collect::<Vec<_>>()
        };
        let spend = encode_spend(&Spend { height: 1, spender });
        assert_eq!(rows(SPENT), vec![(paid.encode().to_vec(), spend.to_vec())], "spent row");
        let bob_receive = ReceiveRow {
            key: ReceiveKey { address: bob, height: 1, txid: spender, vout: 0 },
            value: zat(490),
        };
        let (key, value) = encode_receive(&bob_receive);
        assert_eq!(rows(RECEIVES), vec![(key.to_vec(), value.to_vec())], "receives row");

        tiered.apply(changes);
        let read = reader(&tiered);
        let unspent =
            read.unspent(&[alice, bob, AddressKey::opaque()], 0, usize::MAX).expect("rows");
        let values: Vec<Vec<(u32, u64)>> = unspent
            .iter()
            .map(|rows| rows.iter().map(|row| (row.key.height, row.value.as_u64())).collect())
            .collect();
        assert_eq!(values, vec![vec![], vec![(0, 70), (1, 490)], vec![(0, 1)]], "unspent");
        let alice_receive = read.receives(alice, 0, usize::MAX).expect("rows")[0].key;
        assert_eq!(read.spends_of(&[alice_receive]), vec![Some(Spend { height: 1, spender })]);
    }
}
