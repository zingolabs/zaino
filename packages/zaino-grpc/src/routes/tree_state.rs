//! Tree-state methods (framed once per NFS publish)
//!
//! - `GetTreeState` at heights `<=` [`IndexesAtBlock::answers_through`]
//! - `GetLatestTreeState`, `GetSubtreeRoots` as of the served tip

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use tonic::{body::Body, Status};
use zaino_index_tree_state::{ServeError, TreeStateReader};
use zaino_nfs::{IndexesAtBlock, NfsPublication};
use zaino_persistence::{OverlayView, PinnedView};
use zaino_primitives::types::{SubtreeRoot, Treestate};
use zaino_proto::proto::service;
use zaino_sync::IndexKind;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{BlockHash, ShieldedPool};

use super::args;
use super::info::chain_name;
use crate::memo::PerView;
use crate::read_lanes::{Lane, ReadLanes};
use crate::wire::{self, streamed_response, unary_response};

/// Framed answers per NFS publish: the layer heights every synced wallet asks (the tip among
/// them), and each pool's root list as of the tip (sliced per request)
///
/// - keyed on the `NfsPublication` publish, not the `GrpcSnapshot` (that one moves per chain-view
///   fold)
pub(crate) struct Memos<V> {
    tree_states: Memo<V, BlockHeight, Result<Bytes, Status>>,
    subtree_roots: Memo<V, ShieldedPool, Arc<FramedRoots>>,
}

/// One answer kind, keyed by `K`, per NFS publish
type Memo<V, K, T> = PerView<NfsPublication<V>, K, T>;

impl<V> Default for Memos<V> {
    fn default() -> Self {
        Self { tree_states: PerView::default(), subtree_roots: PerView::default() }
    }
}

pub(crate) struct TreeStateReads<V> {
    pub(crate) indexed: Arc<NfsPublication<V>>,
    pub(crate) reader: TreeStateReader<OverlayView<V>>,
    pub(crate) reads: ReadLanes,
    pub(crate) memos: Arc<Memos<V>>,
}

impl<V: PinnedView> TreeStateReads<V> {
    fn served(&self) -> &IndexesAtBlock<V> {
        self.indexed.served()
    }

    /// Highest height this request answers (the tree-state index's, capped at the served tip)
    fn served_through(&self) -> BlockHeight {
        self.served().answers_through(IndexKind::TreeState)
    }

    /// Framed tree state at `height`, once per NFS publish (the synced wallets' tip asks)
    async fn memoized_tree_state(&self, height: BlockHeight) -> Result<Bytes, Status> {
        let (through, params) = (self.served_through(), self.served().params());
        let frame = move |reader: &_| framed_tree_state(reader, height, through, params);
        self.memoized(|memos| &memos.tree_states, height, frame).await?
    }

    /// Framed tree state at `height`, read on the point lane (not memoized)
    async fn read_tree_state(&self, height: BlockHeight) -> Result<Bytes, Status> {
        let (reader, through, params) =
            (self.reader.clone(), self.served_through(), self.served().params());
        let read = move || framed_tree_state(&reader, height, through, params);
        self.reads.read(Lane::Point, read).await?
    }

    /// Framed tree state at `height` only if this index holds `hash` there (a located height can
    /// lag a reorg)
    async fn read_tree_state_of_block(
        &self,
        height: BlockHeight,
        hash: BlockHash,
    ) -> Result<Bytes, Status> {
        let (reader, params) = (self.reader.clone(), self.served().params());
        let read = move || {
            let tree_state = held_tree_state(&reader, height);
            if tree_state.block_hash != hash {
                return Err(Status::not_found(format!(
                    "block {hash} is not the one this index holds at {height} (reorg)"
                )));
            }
            Ok(tree_state_message(&tree_state, params))
        };
        self.reads.read(Lane::Point, read).await?
    }

    /// Every root of `pool` completed at or below the served tip, framed once per NFS publish
    async fn memoized_subtree_roots(&self, pool: ShieldedPool) -> Result<Arc<FramedRoots>, Status> {
        let tip = self.served().tip().height;
        let frame = move |reader: &TreeStateReader<OverlayView<V>>| {
            let mut roots = reader.subtree_roots(pool);
            roots.retain(|root| root.completing.height <= tip);
            Arc::new(FramedRoots::of(&roots))
        };
        self.memoized(|memos| &memos.subtree_roots, pool, frame).await
    }

    /// `compute` on first ask per NFS publish, on the point lane (single flight), then the cached
    /// answer inline
    async fn memoized<K, T>(
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
        let (memos, indexed, reader) =
            (Arc::clone(&self.memos), Arc::clone(&self.indexed), self.reader.clone());
        let compute = move || memo(&memos).get_or_compute(&indexed, key, || compute(&reader));
        self.reads.read(Lane::Point, compute).await
    }
}

/// Tree state at exactly `height`, framed for the wire
///
/// - past the index's own tip = a miss
/// - below Sapling = every pool `""`, not lightwalletd's error (Android asks `batchStart - 1`, so
///   Sapling activation − 1 too: ZA#1422)
fn framed_tree_state<V: PinnedView>(
    reader: &TreeStateReader<OverlayView<V>>,
    height: BlockHeight,
    served_through: BlockHeight,
    params: zaino_nfs::ChainParams,
) -> Result<Bytes, Status> {
    if height > served_through {
        return Err(Status::not_found(ServeError::NotFound { height }.to_string()));
    }
    Ok(tree_state_message(&held_tree_state(reader, height), params))
}

/// `height` <= `answers_through` (a miss = the publication's own hole)
fn held_tree_state<V: PinnedView>(
    reader: &TreeStateReader<OverlayView<V>>,
    height: BlockHeight,
) -> Treestate {
    let held = reader.treestate(height);
    held.unwrap_or_else(|_| panic!("tree-state view holds {height} <= answers_through"))
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
            framed.extend_from_slice(&subtree_root_message(one));
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
        let wide = |value: u32| usize::try_from(value).expect("u32 fits usize");
        let start = wide(start).min(count);
        let end = match max {
            0 => count,
            max => count.min(start.saturating_add(wide(max))),
        };
        let offset = |roots: usize| roots.checked_sub(1).map_or(0, |at| self.ends[at]);
        self.framed.slice(offset(start)..offset(end))
    }
}

/// `GetLatestTreeState`: the served tip's tree state (memoized per NFS publish)
pub(crate) async fn get_latest_tree_state<V: PinnedView>(
    tree_states: &TreeStateReads<V>,
) -> Result<Response<Body>, Status> {
    let tip = tree_states.served().tip().height;
    let tree_state = tree_states.memoized_tree_state(tip).await?;
    Ok(unary_response(tree_state))
}

/// `GetTreeState`
///
/// - hash wins when given (one block across a reorg): block-hash index locates, this index answers
///   only if it holds that same block
/// - by height in the NFS layer (the synced wallets' tip asks): memoized per NFS publish
pub(crate) async fn get_tree_state<V: PinnedView>(
    tree_states: &TreeStateReads<V>,
    block_id: service::BlockId,
) -> Result<Response<Body>, Status> {
    let tree_state = if block_id.hash.is_empty() {
        let height = args::height(block_id.height, "height")?;
        if tree_states.reader.is_above_durable(height) {
            tree_states.memoized_tree_state(height).await?
        } else {
            tree_states.read_tree_state(height).await?
        }
    } else {
        let through = tree_states.served_through();
        let (height, hash) =
            args::locate(tree_states.served(), through, &block_id.hash, "GetTreeState")?;
        tree_states.read_tree_state_of_block(height, BlockHash(hash)).await?
    };
    Ok(unary_response(tree_state))
}

/// `GetSubtreeRoots`: one slice of every root of the pool completed at or below the served tip
///
/// - whole or refused before the first byte (iOS keeps a partial prefix)
/// - `startIndex` past the end = empty `OK` (pepper-sync resumes until it sees one)
pub(crate) async fn get_subtree_roots<V: PinnedView>(
    tree_states: &TreeStateReads<V>,
    request: service::GetSubtreeRootsArg,
) -> Result<Response<Body>, Status> {
    let pool = shielded_pool(request.shielded_protocol)?;
    let roots = tree_states.memoized_subtree_roots(pool).await?;
    let slice = roots.slice(request.start_index, request.max_entries);
    Ok(streamed_response((!slice.is_empty()).then_some(slice).into_iter().collect()))
}

/// Client `shieldedProtocol` → pool; unknown = `UNIMPLEMENTED` (the one non-Sapling refusal the
/// Android SDK survives with its fast sync intact)
fn shielded_pool(protocol: i32) -> Result<ShieldedPool, Status> {
    match service::ShieldedProtocol::try_from(protocol) {
        Ok(service::ShieldedProtocol::Sapling) => Ok(ShieldedPool::Sapling),
        Ok(service::ShieldedProtocol::Orchard) => Ok(ShieldedPool::Orchard),
        Ok(service::ShieldedProtocol::Ironwood) => Ok(ShieldedPool::Ironwood),
        Err(_) => Err(Status::unimplemented(format!(
            "shieldedProtocol {protocol} is not served (sapling = 0, orchard = 1, ironwood = 2)"
        ))),
    }
}

/// Domain tree state → wire `TreeState`, framed (trees hex, hash in display order)
///
/// - pool below its upgrade = `""` (zebra's `z_gettreestate` omits it, lightwalletd copies that)
/// - from its upgrade on = its tree, `000000` while empty (pepper-sync rejects `""` there)
fn tree_state_message(state: &Treestate, params: zaino_nfs::ChainParams) -> Bytes {
    let activations = params.activations();
    let active = |pool| activations.get(pool).is_some_and(|from| from <= state.height);
    let tree = |pool| match active(pool) {
        true => const_hex::encode(state.trees.get(pool).as_bytes()),
        false => String::new(),
    };
    wire::frame(&service::TreeState {
        network: chain_name(params.network).to_owned(),
        height: u64::from(state.height),
        hash: state.block_hash.to_string(),
        time: state.time,
        sapling_tree: tree(ShieldedPool::Sapling),
        orchard_tree: tree(ShieldedPool::Orchard),
        ironwood_tree: tree(ShieldedPool::Ironwood),
    })
}

/// Domain subtree root → wire `SubtreeRoot`, framed (`completingBlockHash` in display order:
/// lightwalletd reverses the internal-order hash)
fn subtree_root_message(root: &SubtreeRoot) -> Bytes {
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
    use prost::Message as _;
    use tonic::Code;
    use zaino_chainview::TrustedValidatorSet;
    use zaino_primitives::types::{BlockRef, SubtreeRoot, TreeRoot};
    use zaino_proto::proto::service;
    use zaino_source::testing::{all_at, h, MockChain};
    use zaino_sync::IndexKind;
    use zcash_protocol::consensus::NetworkUpgrade;
    use zcash_protocol::local_consensus::LocalNetwork;
    use zcash_protocol::BlockHash;

    use crate::service::Method;
    use crate::testing::{
        answer, decode_frames, dispatch, fixed_snapshots, framed_request, indexed, routes_serving,
        MAINNET,
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

    /// No subtree complete yet: each pool an empty `OK` stream, status in trailers (pepper-sync's
    /// resume probe); start / max slicing = `a_root_request_is_one_slice_of_the_framed_list`
    #[tokio::test]
    async fn subtree_roots_before_any_completes_are_empty_per_pool_and_refuse_an_unknown_one() {
        use tower::Service as _;

        let chain = MockChain::regtest()
            .network(MAINNET)
            .upgrades(all_at(h(0)))
            .genesis_with(|b| b.coinbase(|c| c.sapling_output(1)));
        let genesis = chain.genesis();
        let views = vec![indexed(IndexKind::TreeState, &chain, genesis)];
        let snapshots =
            fixed_snapshots(&chain, genesis, views, TrustedValidatorSet::default(), &[]);
        let mut router = dispatch(routes_serving(MAINNET, &[IndexKind::TreeState], snapshots));

        let (sapling, orchard, ironwood) = (0, 1, 2);
        // case, shieldedProtocol, startIndex, maxEntries, code
        #[rustfmt::skip]
        let cases = [
            ("sapling",                 sapling,  0,        0,        Code::Ok),
            ("orchard after sapling",   orchard,  0,        0,        Code::Ok),
            ("ironwood",                ironwood, 0,        0,        Code::Ok),
            ("unknown pool 3",          3,        0,        0,        Code::Unimplemented),
        ];
        for (case, shielded_protocol, start_index, max_entries, code) in cases {
            let arg = service::GetSubtreeRootsArg { start_index, shielded_protocol, max_entries };

            let response = router
                .call(framed_request(Method::GetSubtreeRoots, arg.encode_to_vec().into()))
                .await;

            let response = response.expect("infallible");
            let trailers_only = response.headers().contains_key("grpc-status");
            let (chunks, status) = answer(response).await;
            assert_eq!(status.code(), code, "{case}: {status:?}");
            assert_eq!(chunks, Vec::<bytes::Bytes>::new(), "{case}: no complete root, no prefix");
            assert_eq!(trailers_only, code != Code::Ok, "{case}: refusal alone = Trailers-Only");
        }
    }

    /// Z#1622: each pool `""` below its upgrade, its tree from it on (`000000` while empty:
    /// pepper-sync rejects `""` there); below Sapling never an error (Android asks Sapling − 1)
    #[tokio::test]
    async fn get_tree_state_by_height_follows_the_activation_schedule_and_latest_is_the_tip() {
        use tower::Service as _;

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
        let views = vec![indexed(IndexKind::TreeState, &chain, tip)];
        let snapshots = fixed_snapshots(&chain, tip, views, TrustedValidatorSet::default(), &[]);
        let mut router = dispatch(routes_serving(MAINNET, &[IndexKind::TreeState], snapshots));
        let mut ask = async |method: Method, message: Vec<u8>| {
            let response = router.call(framed_request(method, message.into())).await;
            let (chunks, status) = answer(response.expect("infallible")).await;
            assert_eq!(status.code(), Code::Ok, "{method:?}: {status:?}");
            let states = decode_frames::<service::TreeState>(&chunks.concat());
            let [state] = <[service::TreeState; 1]>::try_from(states)
                .unwrap_or_else(|states| panic!("{method:?}: one TreeState, got {states:?}"));
            state
        };
        // (time, display-order hash, sapling tree, orchard tree, ironwood tree) of block `at`
        let tree_state = |at: u32, sapling: &str, orchard: &str, ironwood: &str| {
            let header = chain.block(chain.at(h(at)).hash).header().clone();
            (
                u64::from(at),
                header.time,
                header.hash.to_string(),
                sapling.to_owned(),
                orchard.to_owned(),
                ironwood.to_owned(),
            )
        };
        let fields = |state: service::TreeState| {
            let sapling = if state.sapling_tree.len() > "000000".len() {
                "leaves"
            } else {
                &state.sapling_tree
            };
            let sapling = sapling.to_owned();
            (state.height, state.time, state.hash, sapling, state.orchard_tree, state.ironwood_tree)
        };

        #[rustfmt::skip]
        let schedule = [
            ("below Sapling",          tree_state(0, "",       "",       "")),
            ("Sapling from 1",         tree_state(1, "leaves", "",       "")),
            ("Orchard (NU5) from 2",   tree_state(2, "leaves", "000000", "")),
            ("Ironwood (NU6.3) from 3", tree_state(3, "leaves", "000000", "000000")),
        ];
        for (case, expected) in &schedule {
            let at = service::BlockId { height: expected.0, hash: Vec::new() }.encode_to_vec();
            assert_eq!(fields(ask(Method::GetTreeState, at).await), *expected, "{case}");
        }

        let latest = ask(Method::GetLatestTreeState, Vec::new()).await;
        assert_eq!(fields(latest), schedule[3].1, "GetLatestTreeState: the tip's");
        let mut rendered = async || {
            let response =
                router.call(framed_request(Method::GetLatestTreeState, bytes::Bytes::new())).await;
            let (mut chunks, _) = answer(response.expect("infallible")).await;
            chunks.remove(0)
        };
        let (first, second) = (rendered().await, rendered().await);
        assert_eq!(first.as_ptr(), second.as_ptr(), "one render per NFS publish, shared");
    }

    #[tokio::test]
    async fn get_tree_state_by_hash_answers_only_where_the_block_hash_index_holds_that_block() {
        use tower::Service as _;

        // every upgrade from genesis: its one sapling output (leaf → txid → hash) lands at 0
        let at_genesis = |leaf: u32| {
            MockChain::regtest()
                .network(MAINNET)
                .upgrades(all_at(h(0)))
                .genesis_with(|b| b.coinbase(|c| c.sapling_output(leaf)))
        };
        let (chain, other) = (at_genesis(7), at_genesis(8));
        let genesis = chain.genesis();
        let trees = indexed(IndexKind::TreeState, &chain, genesis);
        let indexes = [IndexKind::TreeState, IndexKind::BlockHash];
        let located_by = |locator: Option<&MockChain>| {
            let locator = locator.map(|at| indexed(IndexKind::BlockHash, at, at.genesis()));
            let views = [Some(trees.clone()), locator].into_iter().flatten().collect();
            let snapshots =
                fixed_snapshots(&chain, genesis, views, TrustedValidatorSet::default(), &[]);
            dispatch(routes_serving(MAINNET, &indexes, snapshots))
        };
        let get_tree_state = async |mut router: crate::service::Dispatch<_, _>, hash: Vec<u8>| {
            let message = service::BlockId { height: 0, hash }.encode_to_vec();
            let response = router.call(framed_request(Method::GetTreeState, message.into())).await;
            answer(response.expect("infallible")).await
        };
        let (by_height, _) = get_tree_state(located_by(None), Vec::new()).await;

        #[rustfmt::skip]
        let cases = [
            ("no block-hash locator",     located_by(None),         genesis.hash.0,         Code::Unimplemented),
            ("located",                   located_by(Some(&chain)), genesis.hash.0,         Code::Ok),
            ("unknown hash",              located_by(Some(&chain)), [0xee; 32],             Code::NotFound),
            ("another block located at 0", located_by(Some(&other)), other.genesis().hash.0, Code::NotFound),
        ];
        for (case, router, hash, code) in cases {
            let (answered, status) = get_tree_state(router, hash.to_vec()).await;

            assert_eq!(status.code(), code, "{case}: {status:?}");
            if code == Code::Ok {
                assert_eq!(answered, by_height, "{case}: same record as by height");
            }
        }
    }
}
