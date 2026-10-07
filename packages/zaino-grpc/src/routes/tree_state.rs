//! Tree-state methods: `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` (framed once per
//! publication)

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use tonic::{body::Body, Status};
use zaino_index_tree_state::{ReadView, ServeError, TreeStateService};
use zaino_internal_block_hash_to_height::BlockHashService;
use zaino_persistence::{MapRead, SequenceRead};
use zaino_primitives::network::chain_name;
use zaino_primitives::types::{
    BlockHash, CommitmentTreeBytes, Height, ShieldedPool, SubtreeRoot, Treestate,
};
use zaino_proto::proto::service as proto;

use crate::limits::Lane;
use crate::limits::ReadLanes;
use crate::memo::PerView;
use crate::wire::{self, path, status_response, streamed_response, unary_response};

/// Framed answers per publication: the non-finalized heights every synced wallet asks, the
/// tip, and each pool's whole root list (sliced per request)
pub(crate) struct Memos<V> {
    states: ViewMemo<V, State, Result<Bytes, Status>>,
    roots: ViewMemo<V, ShieldedPool, Result<Arc<FramedRoots>, Status>>,
}

/// One answer kind, keyed by `K`, per published tree-state view
type ViewMemo<V, K, T> = PerView<ReadView<V>, K, T>;

impl<V> Default for Memos<V> {
    fn default() -> Self {
        Self { states: PerView::default(), roots: PerView::default() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum State {
    Latest,
    At(Height),
}

/// Everything one tree-state request is answered with
pub(crate) struct Answering<V> {
    pub(crate) service: TreeStateService<V>,
    pub(crate) locator: Option<BlockHashService<V>>,
    pub(crate) reads: ReadLanes,
    pub(crate) memos: Arc<Memos<V>>,
}

/// Framed on first ask per publication, on the point lane (single flight); then inline
async fn once<V, K, T>(
    memo: fn(&Memos<V>) -> &ViewMemo<V, K, T>,
    answering: &Answering<V>,
    view: Arc<ReadView<V>>,
    key: K,
    compute: impl FnOnce(&ReadView<V>) -> T + Send + 'static,
) -> Result<T, Status>
where
    V: SequenceRead,
    K: Eq + std::hash::Hash + Send + 'static,
    T: Clone + Send + 'static,
{
    if let Some(hit) = memo(&answering.memos).cached(&view, &key) {
        return Ok(hit);
    }
    let memos = Arc::clone(&answering.memos);
    answering
        .reads
        .read(Lane::Point, move || memo(&memos).get_or_compute(&view, key, || compute(&view)))
        .await
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

    /// Roots `start` inclusive to `start + max` exclusive; `max == 0` = to the last root
    /// (`start ≥ count` = none)
    fn slice(&self, start: u16, max: u16) -> Bytes {
        let count = self.ends.len();
        let start = usize::from(start).min(count);
        let end = match max {
            0 => count,
            max => count.min(start + usize::from(max)),
        };
        let offset = |roots: usize| roots.checked_sub(1).map_or(0, |at| self.ends[at]);
        self.framed.slice(offset(start)..offset(end))
    }
}

/// Maps an index error onto a gRPC status.
///
/// `Syncing` = `Unavailable` (clears on its own, so back off and retry; `Unimplemented`
/// would retire the method and `FailedPrecondition` implies the client can fix it).
fn to_status(error: ServeError) -> Status {
    match &error {
        ServeError::Syncing | ServeError::Empty => Status::unavailable(error.to_string()),
        ServeError::NotFound { .. } => Status::not_found(error.to_string()),
        // lightwalletd: "z_gettreestate did not return treestate"
        ServeError::BeforeSapling { .. } => Status::invalid_argument(error.to_string()),
        ServeError::Inconsistent { .. } => Status::internal(error.to_string()),
    }
}

/// Dispatches a claimed tree-state path.
pub(crate) async fn dispatch<V, B>(answering: Answering<V>, path: &str, body: B) -> Response<Body>
where
    V: SequenceRead + MapRead,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let answer = match path {
        path::GET_TREE_STATE => treestate(answering, body).await.map(unary_response),
        path::GET_LATEST_TREE_STATE => latest(&answering).await.map(unary_response),
        path::GET_SUBTREE_ROOTS => subtree_roots(&answering, body).await.map(|chunk| {
            streamed_response((!chunk.is_empty()).then_some(chunk).into_iter().collect())
        }),
        _ => Err(Status::unimplemented("not a tree-state method")),
    };

    match answer {
        Ok(response) => response,
        Err(status) => status_response(status),
    }
}

async fn latest<V: SequenceRead>(answering: &Answering<V>) -> Result<Bytes, Status> {
    let view = answering.service.pin().map_err(to_status)?;
    let service = answering.service.clone();
    once(
        |memos| &memos.states,
        answering,
        view,
        State::Latest,
        move |view| service.latest_in(view).map(|state| reply(&state, &service)).map_err(to_status),
    )
    .await?
}

/// A hash, when given, wins (it names one block across a reorg): the block-hash index locates
/// its height, this index answers there only if it holds that same block
///
/// - by height, non-finalized (the synced wallets' tip asks): once per publication
async fn treestate<V, B>(answering: Answering<V>, body: B) -> Result<Bytes, Status>
where
    V: SequenceRead + MapRead,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let id: proto::BlockId = wire::decode_request(body).await?;

    if id.hash.is_empty() {
        let height = wire::height(id.height, "height")?;
        if let Ok(view) = answering.service.pin() {
            if view.is_non_finalized(height) {
                let service = answering.service.clone();
                return once(
                    |memos| &memos.states,
                    &answering,
                    view,
                    State::At(height),
                    move |view| {
                        service
                            .treestate_in(view, height)
                            .map(|state| reply(&state, &service))
                            .map_err(to_status)
                    },
                )
                .await?;
            }
        }
    }

    let Answering { service, locator, reads, .. } = answering;
    reads.read(Lane::Point, move || treestate_of(&service, locator.as_ref(), &id)).await?
}

fn treestate_of<V: SequenceRead + MapRead>(
    service: &TreeStateService<V>,
    locator: Option<&BlockHashService<V>>,
    id: &proto::BlockId,
) -> Result<Bytes, Status> {
    if id.hash.is_empty() {
        let state = service.treestate(wire::height(id.height, "height")?).map_err(to_status)?;
        return Ok(reply(&state, service));
    }

    let (height, hash) = wire::locate(locator, &id.hash, "GetTreeState")?;
    let state = service.treestate(height).map_err(to_status)?;
    if state.block_hash != BlockHash::from(hash) {
        return Err(Status::not_found(format!(
            "block {} is not the one this index holds at {height} (reorg)",
            BlockHash::from(hash)
        )));
    }
    Ok(reply(&state, service))
}

/// Domain treestate → wire (trees hex, hash in display order)
/// - Pool below its upgrade = `""` (zebra's `z_gettreestate` omits it; lightwalletd copies that)
fn reply<V: SequenceRead>(state: &Treestate, service: &TreeStateService<V>) -> Bytes {
    let activations = service.activations();
    let tree = |pool, tree: &CommitmentTreeBytes| match activations.active(pool, state.height) {
        true => hex::encode(tree.as_bytes()),
        false => String::new(),
    };
    wire::frame(&proto::TreeState {
        network: chain_name(service.network()).to_owned(),
        height: u64::from(state.height),
        hash: state.block_hash.to_string(),
        time: state.time,
        sapling_tree: tree(ShieldedPool::Sapling, &state.sapling),
        orchard_tree: tree(ShieldedPool::Orchard, &state.orchard),
        ironwood_tree: tree(ShieldedPool::Ironwood, &state.ironwood),
    })
}

/// Every root of the pool framed once per publication; a request = one slice of it
async fn subtree_roots<V, B>(answering: &Answering<V>, body: B) -> Result<Bytes, Status>
where
    V: SequenceRead,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: proto::GetSubtreeRootsArg = wire::decode_request(body).await?;

    let pool = match proto::ShieldedProtocol::try_from(request.shielded_protocol) {
        Ok(proto::ShieldedProtocol::Sapling) => ShieldedPool::Sapling,
        Ok(proto::ShieldedProtocol::Orchard) => ShieldedPool::Orchard,
        Ok(proto::ShieldedProtocol::Ironwood) => ShieldedPool::Ironwood,
        Err(_) => {
            return Err(Status::invalid_argument(format!(
                "unknown shieldedProtocol {}",
                request.shielded_protocol
            )))
        }
    };

    // Depth-32 tree = ≤ 2^16 subtrees, so anything wider names none.
    let ceiling = |field: &str| {
        Status::invalid_argument(format!("{field} is above the 2^16 subtree ceiling"))
    };
    let start = u16::try_from(request.start_index).map_err(|_| ceiling("startIndex"))?;
    let max = u16::try_from(request.max_entries).map_err(|_| ceiling("maxEntries"))?;

    let view = answering.service.pin().map_err(to_status)?;
    let framed = once(
        |memos| &memos.roots,
        answering,
        view,
        pool,
        move |view| {
            let every = view.subtree_roots(pool, 0, 0).map_err(to_status)?;
            Ok(Arc::new(FramedRoots::of(&every)))
        },
    )
    .await??;
    Ok(framed.slice(start, max))
}

/// `completingBlockHash` in display order (lightwalletd reverses the internal-order hash)
fn root(root: &SubtreeRoot) -> Bytes {
    let mut hash = <[u8; 32]>::from(root.completing.hash);
    hash.reverse();
    wire::frame(&proto::SubtreeRoot {
        root_hash: <[u8; 32]>::from(root.root).to_vec(),
        completing_block_hash: hash.to_vec(),
        completing_block_height: u64::from(root.completing.height),
    })
}

#[cfg(test)]
mod tests {
    use http::{HeaderValue, Response};
    use prost::Message as _;
    use tonic::{body::Body, Status};
    use zaino_primitives::types::{BlockRef, SubtreeRoot};
    use zaino_proto::frame::{split_frame, FRAME_HEADER};
    use zaino_proto::proto::service as proto;
    use zaino_sync::Served;

    use std::num::NonZeroUsize;

    use zaino_persistence::IndexKind;

    use crate::service::Routes;
    use crate::testing::{dispatch, framed_request, indexed, routes, store};
    use crate::wire::path;

    /// Any `start` inclusive to `start + max` exclusive of the pre-framed list = exactly those
    /// roots, framed as one per record; past the last root = empty (pepper-sync's probe), never
    /// a panic
    #[test]
    fn a_root_request_is_one_slice_of_the_framed_list() {
        let roots: Vec<SubtreeRoot> = (0..5u8)
            .map(|seed| SubtreeRoot {
                root: [seed; 32].into(),
                completing: BlockRef {
                    hash: [seed; 32].into(),
                    height: (u32::from(seed) * 10).try_into().expect("height"),
                },
            })
            .collect();
        let framed = super::FramedRoots::of(&roots);
        let heights = |chunk: bytes::Bytes| {
            let mut found = Vec::new();
            let mut rest = &chunk[..];
            while !rest.is_empty() {
                let (message, tail) = split_frame(rest).expect("whole frame");
                let root = proto::SubtreeRoot::decode(message).expect("decodes");
                found.push(root.completing_block_height);
                rest = tail;
            }
            found
        };

        let cases: [((u16, u16), &[u64]); 6] = [
            ((0, 0), &[0, 10, 20, 30, 40]),
            ((2, 0), &[20, 30, 40]),
            ((1, 2), &[10, 20]),
            ((3, 9), &[30, 40]),
            ((5, 0), &[]),
            ((9, 1), &[]),
        ];
        for ((start, max), expected) in cases {
            assert_eq!(heights(framed.slice(start, max)), expected, "start {start} max {max}");
        }
    }

    /// lightwalletd over zebra's `z_gettreestate`: a pool below its upgrade = `""`, from it = its
    /// tree (`000000` while empty); below Sapling = no tree state at all (`InvalidArgument`)
    #[tokio::test]
    async fn tree_state_fields_follow_the_validators_activation_schedule() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_tree_state::{PoolActivations, TreeStateIndexWriter, TreeStateService};
        use zaino_primitives::testing::Chain;
        use zaino_primitives::types::{
            CompactCiphertext, Height, SaplingData, SaplingOutput, Transaction, TransactionId,
        };
        use zaino_proto::proto::service as proto;

        let net = zcash_protocol::consensus::NetworkType::Regtest;
        let store = store("/ts", &zaino_index_tree_state::schema(net));
        let index = TreeStateIndexWriter::new(store, std::num::NonZeroUsize::MIN).expect("new");
        let served = index.published().served();
        let mut cmu = [0u8; 32];
        cmu[0] = 7;
        let one_output = |seed: u8| Transaction {
            txid: TransactionId::from([seed; 32]),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: SaplingData {
                outputs: vec![SaplingOutput {
                    cmu: cmu.into(),
                    ephemeral_key: [2u8; 32].into(),
                    enc_ciphertext: [3u8; CompactCiphertext::LENGTH].into(),
                }],
                ..Default::default()
            },
            orchard: Default::default(),
            ironwood: Default::default(),
        };
        let mut chain = Chain::with_genesis(vec![one_output(0x40)]);
        let tip = (1..4u8).fold(chain.genesis(), |tip, height| {
            chain.mine_with(tip.hash, vec![one_output(0x40 + height)])
        });
        let blocks: Vec<_> = chain.path(tip.hash).into_iter().map(std::sync::Arc::new).collect();
        indexed(&blocks, IndexKind::TreeState.name(), |queue| index.run(queue)).await;

        let h = |n: u32| Height::try_from(n).expect("h");
        let activations =
            PoolActivations { sapling: h(1), orchard: Some(h(2)), ironwood: Some(h(3)) };
        let service = TreeStateService::new(
            Served::fixed((*served.pin_any()).clone()),
            zcash_protocol::consensus::NetworkType::Regtest,
            activations,
        );
        let mut router = dispatch(Routes { tree_state: Some(service), ..routes() });

        let mut ask = |path: &'static str, body: Vec<u8>| {
            let request = framed_request(path, body.into());
            let call = router.call(request);
            async move {
                use http_body_util::BodyExt as _;
                let response = call.await.expect("router answers");
                let code = Status::from_header_map(response.headers())
                    .map(|status| status.code())
                    .unwrap_or(tonic::Code::Ok);
                let body = response.into_body().collect().await.expect("body").to_bytes();
                let state = (code == tonic::Code::Ok).then(|| {
                    proto::TreeState::decode(&body[FRAME_HEADER..]).expect("one framed message")
                });
                (code, state.map(|s| (s.sapling_tree.is_empty(), s.orchard_tree, s.ironwood_tree)))
            }
        };
        let at = |height| proto::BlockId { height, hash: Vec::new() }.encode_to_vec();

        let empty = || "000000".to_owned();
        let ok =
            |orchard: String, ironwood: String| (tonic::Code::Ok, Some((false, orchard, ironwood)));
        assert_eq!(ask(path::GET_TREE_STATE, at(0)).await, (tonic::Code::InvalidArgument, None));
        assert_eq!(ask(path::GET_TREE_STATE, at(1)).await, ok(String::new(), String::new()));
        assert_eq!(ask(path::GET_TREE_STATE, at(2)).await, ok(empty(), String::new()));
        assert_eq!(ask(path::GET_TREE_STATE, at(3)).await, ok(empty(), empty()));
        assert_eq!(ask(path::GET_LATEST_TREE_STATE, Vec::new()).await, ok(empty(), empty()));
    }

    /// By height, at the tip, and by hash through the compact locator; every pool its own hex tree
    /// (an absent field would read as `CommitmentTree::empty()`)
    #[tokio::test]
    async fn a_populated_tree_state_index_answers_by_height_at_the_tip_and_by_hash() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateService};
        use zaino_primitives::testing::Chain;
        use zaino_primitives::types::{
            Block, CompactCiphertext, SaplingData, SaplingOutput, Transaction, TransactionId,
        };
        use zaino_proto::proto::service as proto;

        // Small little-endian value: canonical under both moduli, unlike a repeated-byte filler.
        let mut cmu = [0u8; 32];
        cmu[0] = 7;

        let net = zcash_protocol::consensus::NetworkType::Regtest;
        let trees = store("/ts", &zaino_index_tree_state::schema(net));
        let index = TreeStateIndexWriter::new(trees, NonZeroUsize::MIN).expect("new");
        let served = index.published().served();

        let chain = Chain::with_genesis(vec![Transaction {
            txid: TransactionId::from([0x33; 32]),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: SaplingData {
                outputs: vec![SaplingOutput {
                    cmu: cmu.into(),
                    ephemeral_key: [2u8; 32].into(),
                    enc_ciphertext: CompactCiphertext::from([3u8; CompactCiphertext::LENGTH]),
                }],
                ..Default::default()
            },
            orchard: Default::default(),
            ironwood: Default::default(),
        }]);
        let block = chain.block(chain.genesis().hash).clone();
        let hash = <[u8; 32]>::from(block.header().hash);
        let name = IndexKind::TreeState.name();
        let blocks = [std::sync::Arc::new(block.clone())];
        indexed(&blocks, name, |queue| index.run(queue)).await;

        let genesis = zaino_primitives::types::Height::GENESIS;
        let service = TreeStateService::new(
            Served::fixed((*served.pin_any()).clone()),
            zcash_protocol::consensus::NetworkType::Regtest,
            zaino_index_tree_state::PoolActivations {
                sapling: genesis,
                orchard: Some(genesis),
                ironwood: Some(genesis),
            },
        );
        let mut router = dispatch(Routes { tree_state: Some(service.clone()), ..routes() });

        async fn tree_state_of(response: Response<Body>) -> proto::TreeState {
            use http_body_util::BodyExt as _;
            let body = response.into_body().collect().await.expect("body").to_bytes();
            proto::TreeState::decode(&body[FRAME_HEADER..]).expect("one framed message")
        }

        let response = router
            .call(framed_request(
                path::GET_TREE_STATE,
                proto::BlockId { height: 0, hash: Vec::new() }.encode_to_vec().into(),
            ))
            .await
            .expect("router answers");
        assert_eq!(response.headers().get("grpc-status"), Some(&HeaderValue::from_static("0")));

        let state = tree_state_of(response).await;
        assert_eq!(state.height, 0);
        assert_eq!(state.time, block.header().time);
        // Display order, like every hash-bearing string field on this wire.
        assert_eq!(state.hash, block.header().hash.to_string());
        assert_ne!(state.sapling_tree, "000000", "the one commitment landed in sapling");
        assert_eq!(state.orchard_tree, "000000", "a real empty tree, not \"\"");
        assert_eq!(state.ironwood_tree, "000000");

        // Tip = the same answer, asked for differently; framed once per publication, so a second
        // ask is the same allocation
        let latest = || async {
            use http_body_util::BodyExt as _;
            let response = router
                .clone()
                .call(framed_request(path::GET_LATEST_TREE_STATE, Vec::new().into()))
                .await
                .expect("router answers");
            let mut body = std::pin::pin!(response.into_body());
            body.frame().await.expect("a frame").expect("ok").into_data().expect("data")
        };
        let (first, second) = (latest().await, latest().await);
        assert_eq!(proto::TreeState::decode(&first[FRAME_HEADER..]).expect("decodes"), state);
        assert_eq!(first.as_ptr(), second.as_ptr(), "one render per publication, shared");

        // By hash: resolved through the block-hash index, so without it, unimplemented
        let by_hash = |hash: [u8; 32]| {
            framed_request(
                path::GET_TREE_STATE,
                proto::BlockId { height: 0, hash: hash.to_vec() }.encode_to_vec().into(),
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
        // (its chain reorged away from the tree-state index's)
        let locator = |block: Block| async move {
            use zaino_internal_block_hash_to_height::BlockHashIndexWriter;
            let schema = zaino_internal_block_hash_to_height::schema(net);
            let index = BlockHashIndexWriter::new(store("/bh", &schema), NonZeroUsize::MIN);
            let served = index.published().served();
            let blocks = [std::sync::Arc::new(block)];
            let name = IndexKind::BlockHash.name();
            indexed(&blocks, name, |queue| index.run(queue)).await;
            let view = (*served.pin_any()).clone();
            zaino_internal_block_hash_to_height::BlockHashService::new(Served::fixed(view))
        };

        let located_by = |locator| {
            let tree_state = Some(service.clone());
            dispatch(Routes { tree_state, block_hash: Some(locator), ..routes() })
        };
        let mut linked = located_by(locator(block).await);
        let response = linked.call(by_hash(hash)).await.expect("answers");
        assert_eq!(status(&response), tonic::Code::Ok);
        assert_eq!(tree_state_of(response).await, state, "same answer as by height");
        let response = linked.call(by_hash([0xee; 32])).await.expect("answers");
        assert_eq!(status(&response), tonic::Code::NotFound, "a hash no index holds");

        // another genesis (another txid → another merkle root → another hash)
        let other = Chain::with_genesis(vec![Transaction {
            txid: TransactionId::from([0x34; 32]),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }]);
        let other = other.block(other.genesis().hash).clone();
        let other_hash = <[u8; 32]>::from(other.header().hash);
        let mut forked = located_by(locator(other).await);
        let response = forked.call(by_hash(other_hash)).await.expect("answers");
        let located = status(&response);
        assert_eq!(located, tonic::Code::NotFound, "located at 0, another block held there");
    }
}
