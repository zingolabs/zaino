//! Tree-state methods (framed once per NFS publish)
//!
//! - `GetTreeState` at heights `<=` [`IndexesAtBlock::answers_through`]
//! - `GetLatestTreeState`, `GetSubtreeRoots` as of the served tip

use std::sync::Arc;

use crate::network::chain_name;
use bytes::Bytes;
use http::Response;
use tonic::{body::Body, Status};
use zaino_index_tree_state::{ServeError, TreeStateReader};
use zaino_nfs::{IndexesAtBlock, NfsPublication};
use zaino_persistence::{IndexKind, OverlayView, PinnedView};
use zaino_primitives::types::{CommitmentTreeBytes, SubtreeRoot, Treestate};
use zaino_proto::proto::service;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{BlockHash, ShieldedPool};

use crate::limits::Lane;
use crate::limits::ReadLanes;
use crate::memo::PerView;
use crate::wire::{self, streamed_response, unary_response};

/// Framed answers per NFS publish: the layer heights every synced wallet asks (the tip among
/// them), and each pool's root list as of the tip (sliced per request)
///
/// - keyed on the `NfsPublication` publish, not the `GrpcSnapshot` (that one moves per chain-view
///   fold)
pub(crate) struct Memos<V> {
    states: Memo<V, BlockHeight, Result<Bytes, Status>>,
    roots: Memo<V, ShieldedPool, Result<Arc<FramedRoots>, Status>>,
}

/// One answer kind, keyed by `K`, per NFS publish
type Memo<V, K, T> = PerView<NfsPublication<V>, K, T>;

impl<V> Default for Memos<V> {
    fn default() -> Self {
        Self { states: PerView::default(), roots: PerView::default() }
    }
}

/// Everything one tree-state request is answered with (`trees` = `indexed`'s served tip's)
pub(crate) struct Answering<V> {
    pub(crate) indexed: Arc<NfsPublication<V>>,
    pub(crate) trees: TreeStateReader<OverlayView<V>>,
    pub(crate) reads: ReadLanes,
    pub(crate) memos: Arc<Memos<V>>,
}

impl<V: PinnedView> Answering<V> {
    fn served(&self) -> &IndexesAtBlock<V> {
        self.indexed.served()
    }

    fn through(&self) -> BlockHeight {
        self.served().answers_through(IndexKind::TreeState)
    }

    /// Framed on first ask per NFS publish, on the point lane (single flight), then inline
    async fn once<K, T>(
        &self,
        memo: fn(&Memos<V>) -> &Memo<V, K, T>,
        key: K,
        compute: impl FnOnce(&TreeStateReader<OverlayView<V>>) -> T + Send + 'static,
    ) -> Result<T, Status>
    where
        K: Eq + std::hash::Hash + Send + 'static,
        T: Clone + Send + 'static,
    {
        if let Some(hit) = memo(&self.memos).cached(&self.indexed, &key) {
            return Ok(hit);
        }
        let (memos, indexed, trees) =
            (Arc::clone(&self.memos), Arc::clone(&self.indexed), self.trees.clone());
        let compute = move || memo(&memos).get_or_compute(&indexed, key, || compute(&trees));
        self.reads.read(Lane::Point, compute).await
    }

    /// Tree state at exactly `at` framed for the wire
    ///
    /// - past the index's own tip = a miss
    /// - below Sapling = every pool `""`, not lightwalletd's error (Android asks `batchStart - 1`,
    ///   so Sapling activation − 1 too: ZA#1422)
    fn state_at(
        &self,
        at: BlockHeight,
    ) -> impl FnOnce(&TreeStateReader<OverlayView<V>>) -> Result<Bytes, Status> + Send + 'static
    {
        let (through, params) = (self.through(), self.served().params());
        move |trees| {
            if at > through {
                return Err(to_status(ServeError::NotFound { height: at }));
            }
            let state = trees.treestate(at).map_err(to_status)?;
            Ok(reply(&state, params))
        }
    }
}

/// One pool's completed roots, framed back to back
///
/// - `ends[i]` = offset just past root `i` (a request = one slice, one DATA chunk)
struct FramedRoots {
    framed: Bytes,
    ends: Vec<usize>,
}

impl FramedRoots {
    fn of(roots: &[SubtreeRoot]) -> Self {
        let mut framed = Vec::new();
        let mut ends = Vec::with_capacity(roots.len());
        for one in roots {
            framed.extend_from_slice(&root(one));
            ends.push(framed.len());
        }
        Self { framed: Bytes::from(framed), ends }
    }

    /// Roots `start` inclusive to `start + max` exclusive
    ///
    /// - `max == 0` = to the last root
    /// - `start ≥ count` = none, any `u32` (pepper-sync resumes until an empty answer)
    fn slice(&self, start: u32, max: u32) -> Bytes {
        let count = self.ends.len();
        let wide = |value: u32| usize::try_from(value).unwrap_or(usize::MAX);
        let start = wide(start).min(count);
        let end = match max {
            0 => count,
            max => count.min(start.saturating_add(wide(max))),
        };
        let offset = |roots: usize| roots.checked_sub(1).map_or(0, |at| self.ends[at]);
        self.framed.slice(offset(start)..offset(end))
    }
}

/// Miss != corruption
fn to_status(error: ServeError) -> Status {
    match &error {
        ServeError::NotFound { .. } => Status::not_found(error.to_string()),
        ServeError::Inconsistent { .. } => Status::internal(error.to_string()),
    }
}

pub(crate) async fn latest<V: PinnedView>(
    answering: &Answering<V>,
) -> Result<Response<Body>, Status> {
    let tip = answering.served().tip().height;
    let state = answering.once(|memos| &memos.states, tip, answering.state_at(tip)).await?;
    state.map(unary_response)
}

/// - Hash wins when given (one block across a reorg): block-hash index locates, this index
///   answers only if it holds that same block
/// - by height, in the NFS layer (the synced wallets' tip asks): once per NFS publish
pub(crate) async fn treestate<V, B>(
    answering: &Answering<V>,
    body: B,
) -> Result<Response<Body>, Status>
where
    V: PinnedView,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    framed_treestate(answering, body).await.map(unary_response)
}

async fn framed_treestate<V, B>(answering: &Answering<V>, body: B) -> Result<Bytes, Status>
where
    V: PinnedView,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let id: service::BlockId = wire::decode_request(body).await?;

    if id.hash.is_empty() {
        let height = wire::height(id.height, "height")?;
        let state = answering.state_at(height);
        if answering.trees.is_above_durable(height) {
            return answering.once(|memos| &memos.states, height, state).await?;
        }
        let trees = answering.trees.clone();
        return answering.reads.read(Lane::Point, move || state(&trees)).await?;
    }

    let (height, hash) =
        wire::locate(answering.served(), answering.through(), &id.hash, "GetTreeState")?;
    let (trees, state) = (answering.trees.clone(), answering.state_at(height));
    let framed = answering.reads.read(Lane::Point, move || {
        let held = trees.treestate(height).map(|state| state.block_hash);
        (held, state(&trees))
    });
    let (held, framed) = framed.await?;
    if held.map_err(to_status)? != BlockHash(hash) {
        return Err(Status::not_found(format!(
            "block {} is not the one this index holds at {height} (reorg)",
            BlockHash(hash)
        )));
    }
    framed
}

/// Domain treestate → wire (trees hex, hash in display order)
///
/// - pool below its upgrade = `""` (zebra's `z_gettreestate` omits it, lightwalletd copies that)
/// - from its upgrade on = its tree, `000000` while empty (pepper-sync rejects `""` there)
fn reply(state: &Treestate, params: zaino_nfs::ChainParams) -> Bytes {
    let activations = params.activations();
    let active = |pool| activations.get(pool).is_some_and(|from| from <= state.height);
    let tree = |pool, tree: &CommitmentTreeBytes| match active(pool) {
        true => const_hex::encode(tree.as_bytes()),
        false => String::new(),
    };
    wire::frame(&service::TreeState {
        network: chain_name(params.network).to_owned(),
        height: u64::from(state.height),
        hash: state.block_hash.to_string(),
        time: state.time,
        sapling_tree: tree(ShieldedPool::Sapling, &state.sapling),
        orchard_tree: tree(ShieldedPool::Orchard, &state.orchard),
        ironwood_tree: tree(ShieldedPool::Ironwood, &state.ironwood),
    })
}

/// Every root of the pool completed at or below the tip, framed once per NFS publish
///
/// - request = one slice of it, whole or refused before the first byte (iOS keeps a partial prefix)
/// - unknown pool = `UNIMPLEMENTED` (the one non-Sapling refusal the Android SDK survives with its
///   fast sync intact)
/// - `startIndex` past the end = empty `OK` (pepper-sync resumes until it sees one)
pub(crate) async fn subtree_roots<V, B>(
    answering: &Answering<V>,
    body: B,
) -> Result<Response<Body>, Status>
where
    V: PinnedView,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: service::GetSubtreeRootsArg = wire::decode_request(body).await?;

    let pool = match service::ShieldedProtocol::try_from(request.shielded_protocol) {
        Ok(service::ShieldedProtocol::Sapling) => ShieldedPool::Sapling,
        Ok(service::ShieldedProtocol::Orchard) => ShieldedPool::Orchard,
        Ok(service::ShieldedProtocol::Ironwood) => ShieldedPool::Ironwood,
        Err(_) => {
            return Err(Status::unimplemented(format!(
                "shieldedProtocol {} is not served (sapling = 0, orchard = 1, ironwood = 2)",
                request.shielded_protocol
            )))
        }
    };
    let (start, max) = (request.start_index, request.max_entries);

    let tip = answering.served().tip().height;
    let every = move |trees: &TreeStateReader<OverlayView<V>>| {
        let mut roots = trees.subtree_roots(pool, 0, 0).map_err(to_status)?;
        roots.retain(|root| root.completing.height <= tip);
        Ok::<_, Status>(Arc::new(FramedRoots::of(&roots)))
    };
    let framed = answering.once(|memos| &memos.roots, pool, every).await?;
    let chunk = framed?.slice(start, max);
    Ok(streamed_response((!chunk.is_empty()).then_some(chunk).into_iter().collect()))
}

/// `completingBlockHash` in display order (lightwalletd reverses the internal-order hash)
fn root(root: &SubtreeRoot) -> Bytes {
    let mut hash = root.completing.hash.0;
    hash.reverse();
    wire::frame(&service::SubtreeRoot {
        root_hash: root.root.0.to_vec(),
        completing_block_hash: hash.to_vec(),
        completing_block_height: u64::from(root.completing.height),
    })
}

#[cfg(test)]
mod tests {
    use http::{HeaderValue, Response};
    use prost::Message as _;
    use tonic::{body::Body, Status};
    use zaino_persistence::IndexKind;
    use zaino_primitives::types::{BlockRef, SubtreeRoot, TreeRoot};
    use zaino_proto::frame::FRAME_HEADER;
    use zaino_proto::proto::service;
    use zaino_source::testing::{all_at, h, MockChain};
    use zcash_protocol::consensus::NetworkUpgrade;
    use zcash_protocol::local_consensus::LocalNetwork;
    use zcash_protocol::BlockHash;

    use crate::service::{Method, RouteDeps};
    use crate::testing::{
        decode_frames, dispatch, framed_request, grpc_snapshots, indexed, routes, MAINNET,
    };

    /// - `start..start + max` of the pre-framed list = exactly those roots, one frame each
    /// - Past the last root = empty (pepper-sync's probe), never a panic
    #[test]
    fn a_root_request_is_one_slice_of_the_framed_list() {
        let roots: Vec<SubtreeRoot> = (0..5u8)
            .map(|seed| SubtreeRoot {
                root: TreeRoot([seed; 32]),
                completing: BlockRef {
                    hash: BlockHash([seed; 32]),
                    height: (u32::from(seed) * 10).into(),
                },
            })
            .collect();
        let framed = super::FramedRoots::of(&roots);
        let heights = |chunk: bytes::Bytes| {
            let roots = decode_frames::<service::SubtreeRoot>(&chunk);
            roots.into_iter().map(|root| root.completing_block_height).collect::<Vec<_>>()
        };

        let cases: [((u32, u32), &[u64]); 9] = [
            ((0, 0), &[0, 10, 20, 30, 40]),
            ((2, 0), &[20, 30, 40]),
            ((1, 2), &[10, 20]),
            ((3, 9), &[30, 40]),
            ((5, 0), &[]),
            ((9, 1), &[]),
            ((0, 100_000), &[0, 10, 20, 30, 40]),
            ((70_000, 0), &[]),
            ((u32::MAX, u32::MAX), &[]),
        ];
        for ((start, max), expected) in cases {
            assert_eq!(heights(framed.slice(start, max)), expected, "start {start} max {max}");
        }
    }

    /// Over the router, no subtree complete yet:
    ///
    /// - every pool, any `startIndex` / `maxEntries` (past `u16` too) = an empty `OK` stream
    ///   (status in trailers: pepper-sync's resume probe)
    /// - Orchard after Sapling = iOS's pass
    /// - unknown pool = `UNIMPLEMENTED`, trailers-only (no body: never a partial prefix)
    #[tokio::test]
    async fn subtree_roots_answer_whole_or_refuse_before_the_first_byte() {
        use http_body_util::BodyExt as _;
        use tower::Service as _;

        let chain = MockChain::regtest()
            .network(MAINNET)
            .upgrades(all_at(h(0)))
            .genesis_with(|b| b.coinbase(|c| c.sapling_output(1)));
        let genesis = chain.genesis();
        let trees = indexed(IndexKind::TreeState, &chain, genesis);
        let mut router = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, genesis, vec![trees]),
            ..routes()
        });

        let (sapling, orchard, ironwood) = (0, 1, 2);
        let ok = || (None, Some("0".to_owned()), 0);
        let cases = [
            ((sapling, 0, 0), ok()),
            ((orchard, 0, 0), ok()),
            ((ironwood, 0, 0), ok()),
            ((sapling, 1, 0), ok()),
            ((orchard, 70_000, 0), ok()),
            ((sapling, u32::MAX, u32::MAX), ok()),
            ((orchard, 0, 100_000), ok()),
            ((3, 0, 0), (Some("12".to_owned()), None, 0)),
        ];
        for ((protocol, start_index, max_entries), expected) in cases {
            let arg = service::GetSubtreeRootsArg {
                start_index,
                shielded_protocol: protocol,
                max_entries,
            };
            let request = framed_request(Method::GetSubtreeRoots, arg.encode_to_vec().into());
            let response = router.call(request).await.expect("router answers");
            let status = |headers: &http::HeaderMap| {
                headers.get("grpc-status").and_then(|s| s.to_str().ok()).map(str::to_owned)
            };
            let headed = status(response.headers());
            let body = response.into_body().collect().await.expect("body");
            let trailed = body.trailers().and_then(status);
            let got = (headed, trailed, body.to_bytes().len());
            assert_eq!(got, expected, "protocol {protocol} start {start_index} max {max_entries}");
        }
    }

    /// Z#1622:
    ///
    /// - exactly the requested height, display-order hash
    /// - each pool `""` below its upgrade, its tree from it on (`000000` while empty: pepper-sync
    ///   rejects `""` there)
    /// - below Sapling = every pool `""`, never an error (Android asks Sapling activation − 1)
    #[tokio::test]
    async fn tree_state_fields_follow_the_validators_activation_schedule() {
        use tower::Service as _;

        // - Sapling at 1, NU5 (orchard) at 2, NU6.3 (ironwood) at 3
        // - one sapling output per block
        let upgrades = LocalNetwork::from_activation_heights(|upgrade| match upgrade {
            NetworkUpgrade::Nu7 => None,
            NetworkUpgrade::Nu6_3 => Some(h(3)),
            NetworkUpgrade::Nu5 | NetworkUpgrade::Nu6 | NetworkUpgrade::Nu6_1 => Some(h(2)),
            NetworkUpgrade::Nu6_2 => Some(h(2)),
            _ => Some(h(1)),
        });
        let mut chain = MockChain::regtest().network(MAINNET).upgrades(upgrades);
        for leaf in 1..=3 {
            chain.mine(|b| b.coinbase(|c| c.sapling_output(leaf)));
        }
        let tip = chain.tip();
        let trees = indexed(IndexKind::TreeState, &chain, tip);
        let mut router = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, tip, vec![trees]),
            ..routes()
        });

        let mut ask = |method: Method, body: Vec<u8>| {
            let request = framed_request(method, body.into());
            let call = router.call(request);
            async move {
                use http_body_util::BodyExt as _;
                let response = call.await.expect("router answers");
                let code = Status::from_header_map(response.headers())
                    .map(|status| status.code())
                    .unwrap_or(tonic::Code::Ok);
                let body = response.into_body().collect().await.expect("body").to_bytes();
                assert_eq!(code, tonic::Code::Ok, "{method:?}");
                let s = service::TreeState::decode(&body[FRAME_HEADER..]).expect("one message");
                let sapling = match s.sapling_tree.as_str() {
                    "" => "",
                    "000000" => "000000",
                    _ => "leaves",
                };
                (s.height, s.hash, sapling, s.orchard_tree, s.ironwood_tree)
            }
        };
        let at = |height| service::BlockId { height, hash: Vec::new() }.encode_to_vec();
        let display = |height: u32| chain.at(h(height)).hash.to_string();

        let (blank, empty) = (String::new, || "000000".to_owned());
        let expected = [
            (0, (0, display(0), "", blank(), blank())),
            (1, (1, display(1), "leaves", blank(), blank())),
            (2, (2, display(2), "leaves", empty(), blank())),
            (3, (3, display(3), "leaves", empty(), empty())),
        ];
        for (height, state) in expected.clone() {
            assert_eq!(ask(Method::GetTreeState, at(height)).await, state, "at {height}");
        }
        let latest = ask(Method::GetLatestTreeState, Vec::new()).await;
        assert_eq!(latest, expected[3].1, "the tip's");
    }

    /// - by height, at the tip, and by hash through the block-hash locator
    /// - every pool its own hex tree (an absent field would read as `CommitmentTree::empty()`)
    #[tokio::test]
    async fn a_populated_tree_state_index_answers_by_height_at_the_tip_and_by_hash() {
        use tower::Service as _;

        // every upgrade from genesis: its one sapling output lands at 0
        let at_genesis = |leaf: u32| {
            MockChain::regtest()
                .network(MAINNET)
                .upgrades(all_at(h(0)))
                .genesis_with(|b| b.coinbase(|c| c.sapling_output(leaf)))
        };
        let chain = at_genesis(7);
        let genesis = chain.genesis();
        let block = chain.block(genesis.hash);
        let hash = genesis.hash.0;
        let trees = indexed(IndexKind::TreeState, &chain, genesis);
        let mut router = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, genesis, vec![trees.clone()]),
            ..routes()
        });

        // A unary answer: data, then `OK` trailers (status in headers beside a body = malformed)
        async fn tree_state_of(response: Response<Body>) -> service::TreeState {
            use http_body_util::BodyExt as _;
            assert_eq!(response.headers().get("grpc-status"), None, "unary status in headers");
            let body = response.into_body().collect().await.expect("body");
            let status = body.trailers().and_then(|trailers| trailers.get("grpc-status").cloned());
            assert_eq!(status, Some(HeaderValue::from_static("0")), "unary");
            let body = body.to_bytes();
            service::TreeState::decode(&body[FRAME_HEADER..]).expect("one framed message")
        }

        let response = router
            .call(framed_request(
                Method::GetTreeState,
                service::BlockId { height: 0, hash: Vec::new() }.encode_to_vec().into(),
            ))
            .await
            .expect("router answers");

        let state = tree_state_of(response).await;
        assert_eq!(state.height, 0);
        assert_eq!(state.time, block.header().time);
        // Display order (every hash-bearing string field on this wire)
        assert_eq!(state.hash, block.header().hash.to_string());
        assert_ne!(state.sapling_tree, "000000", "the one commitment landed in sapling");
        assert_eq!(state.orchard_tree, "000000", "a real empty tree, not \"\"");
        assert_eq!(state.ironwood_tree, "000000");

        // Tip = same answer, framed once per NFS publish (second ask = same allocation)
        let latest = || async {
            use http_body_util::BodyExt as _;
            let response = router
                .clone()
                .call(framed_request(Method::GetLatestTreeState, Vec::new().into()))
                .await
                .expect("router answers");
            let mut body = std::pin::pin!(response.into_body());
            body.frame().await.expect("a frame").expect("ok").into_data().expect("data")
        };
        let (first, second) = (latest().await, latest().await);
        assert_eq!(service::TreeState::decode(&first[FRAME_HEADER..]).expect("decodes"), state);
        assert_eq!(first.as_ptr(), second.as_ptr(), "one render per NFS publish, shared");

        // By hash: resolved through the block-hash index, so without it, unimplemented
        let by_hash = |hash: [u8; 32]| {
            framed_request(
                Method::GetTreeState,
                service::BlockId { height: 0, hash: hash.to_vec() }.encode_to_vec().into(),
            )
        };
        let status = |response: &Response<Body>| {
            Status::from_header_map(response.headers())
                .map(|status| status.code())
                .unwrap_or(tonic::Code::Ok)
        };
        let response = router.call(by_hash(hash)).await.expect("answers");
        assert_eq!(status(&response), tonic::Code::Unimplemented);

        // Block-hash index holding the same block at 0, and one holding another block there
        let located_by = |locator: &MockChain| {
            let views =
                vec![trees.clone(), indexed(IndexKind::BlockHash, locator, locator.genesis())];
            dispatch(RouteDeps {
                grpc_snapshots: grpc_snapshots(&chain, genesis, views),
                ..routes()
            })
        };
        let mut linked = located_by(&chain);
        let response = linked.call(by_hash(hash)).await.expect("answers");
        assert_eq!(status(&response), tonic::Code::Ok);
        assert_eq!(tree_state_of(response).await, state, "same answer as by height");
        let response = linked.call(by_hash([0xee; 32])).await.expect("answers");
        assert_eq!(status(&response), tonic::Code::NotFound, "a hash no index holds");

        // another genesis (another leaf → another txid → another merkle root → another hash)
        let other = at_genesis(8);
        let other_hash = other.genesis().hash.0;
        let mut forked = located_by(&other);
        let response = forked.call(by_hash(other_hash)).await.expect("answers");
        let located = status(&response);
        assert_eq!(located, tonic::Code::NotFound, "located at 0, another block held there");
    }
}
