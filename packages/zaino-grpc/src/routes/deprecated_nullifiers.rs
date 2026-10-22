//! Deprecated `GetBlockRangeNullifiers`: a `GetBlockRange` walk cut to each block's nullifiers

// =================================================================================================
// !!! TODO: REMOVE THIS MODULE. DEPRECATED `GetBlockRangeNullifiers`.
// !!! - Served only for pepper-sync (still calls it)
// !!! - not part of Zaino's supported surface
// !!! - Delete with its `Method` variant + arms and its proto rpc once pepper-sync
// !!!   requests `GetBlockRange` with `poolTypes`
// =================================================================================================

use bytes::Bytes;
use http::Response;
use prost::Message as _;
use tonic::{body::Body, Status};
use zaino_proto::frame::frame_into;
use zaino_proto::proto::compact_formats;
use zaino_proto::proto::service;

use super::args;
use super::blocks::CompactBlocks;
use crate::read_lanes::ReadLanes;
use crate::wire;

/// `GetBlockRangeNullifiers`
///
/// - proto: MUST ignore a `TRANSPARENT` member (dropped before projection → `[TRANSPARENT]` alone
///   = empty = shielded default, not "no pools")
pub(crate) fn get_block_range_nullifiers<V: zaino_persistence::PinnedView>(
    compact_blocks: CompactBlocks<'_, V>,
    mut range: service::BlockRange,
    reads: ReadLanes,
) -> Result<Response<Body>, Status> {
    range.pool_types.retain(|pool| *pool != service::PoolType::Transparent as i32);

    let pools = args::pools(&range.pool_types)?;
    let blocks = compact_blocks.open_range(&range, pools)?;
    Ok(blocks.into_response(reads, nullifiers_only_chunk))
}

/// Framed records → framed nullifier-only records, one frame per block
fn nullifiers_only_chunk(chunk: Bytes) -> Result<Bytes, Status> {
    let mut out = Vec::with_capacity(chunk.len());
    let messages = wire::messages(&chunk)
        .ok_or_else(|| Status::internal("stored range chunk is not whole frames"))?;
    for message in messages {
        let block = compact_formats::CompactBlock::decode(message)
            .map_err(|e| Status::internal(e.to_string()));
        let block = nullifiers_only(block?);
        frame_into(&mut out, |out| block.encode_raw(out));
    }

    Ok(Bytes::from(out))
}

/// Spends whole (a spend = its nullifier record), actions cut to `nullifier`, rest emptied
fn nullifiers_only(mut block: compact_formats::CompactBlock) -> compact_formats::CompactBlock {
    let cut = |actions: Vec<compact_formats::CompactOrchardAction>| {
        actions
            .into_iter()
            .map(|action| compact_formats::CompactOrchardAction {
                nullifier: action.nullifier,
                ..Default::default()
            })
            .collect()
    };

    for tx in &mut block.vtx {
        tx.outputs.clear();
        tx.vin.clear();
        tx.vout.clear();
        tx.actions = cut(std::mem::take(&mut tx.actions));
        tx.ironwood_actions = cut(std::mem::take(&mut tx.ironwood_actions));
    }
    // proto: commitment tree sizes not included
    block.chain_metadata = Some(compact_formats::ChainMetadata::default());

    block
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use zaino_persistence::IndexKind;
    use zaino_source::testing::MockChain;

    use crate::service::{Method, RouteDeps};
    use crate::testing::{
        decode_frames, dispatch, drained, framed_request, grpc_snapshots, indexed, routes, MAINNET,
    };

    /// TODO: REMOVE with the deprecated alias. Blocks 1..=4, each one tx spending sapling
    /// nullifier `0x30 + h`, orchard `0x70 + h`, ironwood `0x80 + h` and `0x90 + h`:
    /// `[TRANSPARENT]` ignored (shielded default), each block = its nullifiers, all else gone
    #[tokio::test]
    async fn get_block_range_nullifiers_answers_only_each_blocks_nullifiers() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::compact_formats;
        use zaino_proto::proto::service;
        use zaino_source::testing::{nullifier, p2pkh};

        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().network(MAINNET);
        for at in 1..=4u32 {
            chain.mine(|b| {
                b.coinbase(|c| c.pay(&alice, 17_345)).tx(|t| {
                    t.spend_in_block(0, 0)
                        .pay(&alice, 12_345)
                        .fee(5_000)
                        .sapling_spend(0x30 + at)
                        .sapling_output(at)
                        .orchard_action(0x70 + at, at)
                        .ironwood_action(0x80 + at, 2 * at)
                        .ironwood_action(0x90 + at, 2 * at + 1)
                })
            });
        }
        let tip = chain.tip();
        let compact = indexed(IndexKind::CompactBlock, &chain, tip);
        let mut router = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, tip, vec![compact]),
            ..routes()
        });

        let range = service::BlockRange {
            start: Some(service::BlockId { height: 1, hash: Vec::new() }),
            end: Some(service::BlockId { height: 4, hash: Vec::new() }),
            pool_types: vec![service::PoolType::Transparent as i32],
        };
        let response = router
            .call(framed_request(Method::GetBlockRangeNullifiers, range.encode_to_vec().into()))
            .await
            .expect("router answers");
        let (chunks, trailing) = drained(response).await;
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));

        // per tx: (sapling spends, sapling outputs, transparent in + out, orchard, ironwood)
        let projected: Vec<_> = decode_frames::<compact_formats::CompactBlock>(&chunks.concat())
            .into_iter()
            .map(|block| {
                let txs: Vec<_> = block
                    .vtx
                    .into_iter()
                    .map(|tx| {
                        let spends: Vec<_> = tx.spends.into_iter().map(|spend| spend.nf).collect();
                        let transparent = tx.vin.len() + tx.vout.len();
                        (spends, tx.outputs.len(), transparent, tx.actions, tx.ironwood_actions)
                    })
                    .collect();
                (block.height, block.chain_metadata, txs)
            })
            .collect();
        let action = |tag: u32| compact_formats::CompactOrchardAction {
            nullifier: nullifier(tag).0.to_vec(),
            ..Default::default()
        };
        let expected: Vec<_> = (1..=4u32)
            .map(|at| {
                let spends = vec![nullifier(0x30 + at).0.to_vec()];
                let ironwood = vec![action(0x80 + at), action(0x90 + at)];
                let tx = (spends, 0, 0, vec![action(0x70 + at)], ironwood);
                (u64::from(at), Some(compact_formats::ChainMetadata::default()), vec![tx])
            })
            .collect();
        assert_eq!(projected, expected);
    }
}
