//! Header chain → producer → followers against validators whose best chain moves at random,
//! checked against the header chain itself
//!
//! - Moves: extensions, reorgs onto heavier branches (longer, same height, retreats), bursts that
//!   never settle, restarts with the chain moving while down, a validator unreachable, the
//!   lagging one stalling
//! - Three validators: two serve the best chain, one the previous move's (by hash, best chain
//!   only, as zebrad: the producer has to route around it)
//! - Header chain finalized after every move (depth `DEPTH`): the producer's one finality
//! - Every commit checked as it lands: final in the header chain, on its best chain
//! - Paused clock: retries and hedges cost no wall time

use std::{
    num::{NonZeroU32, NonZeroUsize},
    sync::{Arc, Mutex},
    time::Duration,
};

use proptest::prelude::*;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zaino_header_chain::{HeaderChain, VerifiedChain};
use zaino_primitives::testing;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::mock::MockChain;
use zaino_sync::{BlockSink, ProduceError, Producer, Step, Subscription, Weight};

const DEPTH: u32 = 4;
/// Virtual time (paused clock)
const SETTLE: Duration = Duration::from_secs(600);

/// Header chain + the best path and final height it verified last
struct Chain {
    headers: HeaderChain,
    best: Vec<BlockHash>,
    final_height: Option<usize>,
}

impl Chain {
    fn refresh(&mut self) -> Arc<VerifiedChain> {
        let verified = self.headers.verified().expect("genesis verified");
        let best = u32::from(verified.best().height);
        let at = |h: u32| verified.hash_at(Height::try_from(h).expect("h")).expect("best path");
        self.best = (0..=best).map(at).collect();
        self.final_height = verified.final_tip().map(|tip| u32::from(tip.height) as usize);
        Arc::new(verified)
    }
}

/// What one index holds: committed hashes, and applied ones (committed prefix + non-finalized)
#[derive(Debug, Default)]
struct Held {
    durable: Vec<BlockHash>,
    applied: Vec<BlockHash>,
}

/// An index whose loop is the real indexes' (block_hash's), recording into `held`: asserts the
/// contract the real indexes assert, and checks each commit against the chain as it lands
struct Recorder {
    held: Arc<Mutex<Held>>,
    chain: Arc<Mutex<Chain>>,
    bulk: Vec<Arc<Block>>,
    bulk_bytes: usize,
    batch_bytes: NonZeroUsize,
}

impl Recorder {
    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().expect("recorder lock")
    }

    async fn run(mut self, mut blocks: Subscription<Block>) {
        loop {
            match blocks.next().await {
                Step::Apply { height, finalized: true, data } => {
                    // replay for an index behind this one: already committed
                    if Some(height) <= tip_of(&self.held().durable) {
                        continue;
                    }
                    self.bulk_bytes += data.weight();
                    self.bulk.push(data);
                    if self.bulk_bytes >= self.batch_bytes.get() {
                        self.commit(height);
                    }
                }
                Step::Apply { height, finalized: false, data } => {
                    if let Some(last) = self.bulk.last() {
                        self.commit(last.header().height);
                    }
                    let mut held = self.held();
                    assert_eq!(height.checked_sub(1), tip_of(&held.applied), "apply gap");
                    held.applied.push(data.header().hash);
                }
                Step::Finalized { height } => self.commit(height),
                Step::Reorg => {
                    assert!(self.bulk.is_empty(), "reorg with bulk blocks staged");
                    let mut held = self.held();
                    held.applied = held.durable.clone();
                }
                Step::Shutdown => {
                    if let Some(last) = self.bulk.last() {
                        self.commit(last.header().height);
                    }
                    return;
                }
            }
        }
    }

    /// Every final block through `through` (bulk ones, then applied ones) committed: contiguous
    /// from durable, on the applied branch where applied, final in the header chain, on its best
    /// chain
    fn commit(&mut self, through: Height) {
        let bulk = std::mem::take(&mut self.bulk);
        self.bulk_bytes = 0;
        let (from, hashes) = {
            let held = self.held();
            let from = held.durable.len();
            let mut hashes: Vec<BlockHash> = Vec::new();
            for (at, block) in (from..).zip(&bulk) {
                assert_eq!(u32::from(block.header().height) as usize, at, "finalize gap");
                if let Some(applied) = held.applied.get(at) {
                    assert_eq!(*applied, block.header().hash, "finalized over another branch");
                }
                hashes.push(block.header().hash);
            }
            let through = u32::from(through) as usize;
            for at in from + hashes.len()..=through {
                hashes.push(*held.applied.get(at).expect("finalized a block never applied"));
            }
            assert_eq!(from + hashes.len(), through + 1, "committed past {through}");
            (from, hashes)
        };
        // checked before `held` is locked (a panic under it poisons every later read)
        {
            let chain = self.chain.lock().expect("chain lock");
            for (at, hash) in (from..).zip(&hashes) {
                assert!(Some(at) <= chain.final_height, "{at} committed before final");
                assert_eq!(chain.best.get(at), Some(hash), "{at} committed off the best chain");
            }
        }
        let mut held = self.held();
        held.durable.extend(hashes);
        if held.applied.len() < held.durable.len() {
            held.applied = held.durable.clone();
        }
    }
}

fn durable_tip(held: &Held) -> Option<BlockRef> {
    let hash = held.durable.last().copied()?;
    Some(BlockRef { hash, height: tip_of(&held.durable)? })
}

/// Last height of `hashes` held from genesis, inclusive (`None` = none held)
fn tip_of(hashes: &[BlockHash]) -> Option<Height> {
    let last = u32::try_from(hashes.len()).expect("small chain").checked_sub(1)?;
    Some(Height::try_from(last).expect("small chain"))
}

/// `Reorg` = top `depth` replaced by `len` heavier blocks (`len` < `depth` = a retreat)
#[derive(Debug, Clone)]
enum Change {
    Extend(u32),
    Reorg { depth: u32, len: u32 },
}

/// - `Chain.yields`: `None` → wait for every index to reach the new chain, `Some(n)` → `n` yields
/// - `Restart` = stop everything, move the chain while down, start from what is durable
/// - `Lag` = first current validator unreachable for `yields`, then back (chain fixed)
/// - `Stall(true)` = lagging validator stops following the chain (`false` = resumes)
#[derive(Debug, Clone)]
enum Move {
    Chain { change: Change, yields: Option<u8> },
    Restart { down: Vec<Change> },
    Lag { yields: u8 },
    Stall(bool),
}

/// One index: blocks per commit, blocks its queue holds
#[derive(Debug, Clone, Copy)]
struct Follower {
    batch: usize,
    budget: usize,
}

fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        2 => (1u32..=DEPTH + 3).prop_map(Change::Extend),
        3 => (1u32..=DEPTH, 1u32..=2 * DEPTH + 2)
            .prop_map(|(depth, len)| Change::Reorg { depth, len }),
    ]
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    prop::collection::vec(
        prop_oneof![
            8 => (change(), prop::option::weighted(0.6, Just(())))
                .prop_flat_map(|(change, settle)| {
                    let yields = match settle {
                        Some(()) => Just(None).boxed(),
                        None => (0u8..=16).prop_map(Some).boxed(),
                    };
                    (Just(change), yields).prop_map(|(change, yields)| Move::Chain { change, yields })
                }),
            1 => prop::collection::vec(change(), 0..3).prop_map(|down| Move::Restart { down }),
            2 => (0u8..=16).prop_map(|yields| Move::Lag { yields }),
            1 => any::<bool>().prop_map(Move::Stall),
        ],
        1..16,
    )
}

fn followers() -> impl Strategy<Value = Vec<Follower>> {
    prop::collection::vec(
        (1usize..=4, 1usize..=6).prop_map(|(batch, budget)| Follower { batch, budget }),
        1..=3,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// After every settled move each index applies exactly the best chain; no index ever commits
    /// a block the header chain has not made final, or that is off its best chain
    #[test]
    fn followers_track_the_best_chain_through_random_reorgs_and_restarts(
        moves in moves(),
        followers in followers(),
        lagging_at in 0usize..3,
        concurrency in 1usize..=4,
    ) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime")
            .block_on(run(moves, followers, lagging_at, concurrency));
    }
}

/// Validators + every block ever mined (the lagging one replays a chain from these)
struct Validators {
    current: [Arc<MockChain>; 2],
    lagging: Arc<MockChain>,
    order: Vec<Arc<MockChain>>,
    blocks: testing::Chain,
    stalled: bool,
}

impl Validators {
    fn serve(mock: &MockChain, best: &[BlockHash], blocks: &testing::Chain) {
        mock.rewind_to(Height::try_from(0u32).expect("h"));
        mock.extend_best(best.iter().map(|own| blocks.block(*own).clone()));
    }

    /// `change` mined, verified and finalized (fork parent clamped to the final tip); the
    /// lagging validator keeps the chain from before it (unless stalled: then whatever it had)
    fn apply(&mut self, chain: &Mutex<Chain>, change: &Change) {
        let mut chain = chain.lock().expect("chain lock");
        let before = chain.best.clone();
        let (keep, tip) = match *change {
            Change::Extend(count) => {
                (before.len(), self.blocks.extend(before[before.len() - 1], count))
            }
            Change::Reorg { depth, len } => {
                let floor = chain.final_height.map_or(1, |h| h + 1);
                let keep = before.len().saturating_sub(depth as usize).max(floor);
                // nested reorgs past the u128 work range: skipped
                let Some(heavy) = self.blocks.mine_heavier(before[keep - 1], &before[keep..])
                else {
                    return;
                };
                (keep, self.blocks.extend(heavy.hash, len - 1))
            }
        };
        let tip = tip.hash;
        chain.headers.insert_blocks(&self.blocks.path(tip)[keep..]).expect("valid headers");
        if let Some(boundary) = chain.headers.finalizable() {
            chain.headers.finalize(boundary).expect("in-memory store");
        }
        chain.refresh();
        assert_eq!(chain.best.last(), Some(&tip), "the heavier branch is best");
        for current in &self.current {
            Self::serve(current, &chain.best, &self.blocks);
        }
        if !self.stalled {
            Self::serve(&self.lagging, &before, &self.blocks);
        }
    }
}

/// One running producer + its followers, fed the header chain
struct Running {
    verified: watch::Sender<Option<Arc<VerifiedChain>>>,
    cancel: CancellationToken,
    producer: JoinHandle<Result<(), ProduceError>>,
    followers: Vec<JoinHandle<()>>,
}

fn start(
    validators: &Validators,
    chain: &Arc<Mutex<Chain>>,
    held: &[Arc<Mutex<Held>>],
    followers: &[Follower],
    concurrency: usize,
) -> Running {
    // every block a bare coinbase: one weight
    let weight = validators.blocks.block(validators.blocks.genesis().hash).weight();
    let mut block_sink = BlockSink::new("blocks");
    let (verified, verified_rx) = watch::channel(Some(chain.lock().expect("lock").refresh()));
    let cancel = CancellationToken::new();
    let (mut spawned, mut durable) = (Vec::new(), Vec::new());
    for (held, follower) in held.iter().zip(followers) {
        durable.push(durable_tip(&held.lock().expect("lock")));
        let budget = NonZeroUsize::new(follower.budget * (weight + 64)).expect("> 0");
        let subscription = block_sink.subscribe("recorder", budget);
        let batch = NonZeroUsize::new(follower.batch * weight).expect("> 0");
        let (held, chain) = (Arc::clone(held), Arc::clone(chain));
        let recorder =
            Recorder { held, chain, bulk: Vec::new(), bulk_bytes: 0, batch_bytes: batch };
        spawned.push((recorder, subscription));
    }
    let concurrency = NonZeroUsize::new(concurrency).expect("1..=4");
    let sources = validators.order.clone();
    let producer = Producer::new(block_sink, sources, verified_rx, concurrency, durable);
    let producer = tokio::spawn(producer.run(cancel.clone()));
    let followers =
        spawned.into_iter().map(|(recorder, blocks)| tokio::spawn(recorder.run(blocks))).collect();
    Running { verified, cancel, producer, followers }
}

async fn stop(running: Running, context: &str) {
    running.cancel.cancel();
    let produced = running.producer.await.expect("producer join");
    assert!(produced.is_ok(), "{context}: producer {produced:?}");
    for task in running.followers {
        task.await.unwrap_or_else(|panic| panic!("{context}: task {panic:?}"));
    }
}

/// Every index applied the best chain, durable through the final tip (within one batch)
async fn settle(
    running: &mut Running,
    chain: &Mutex<Chain>,
    held: &[Arc<Mutex<Held>>],
    followers: &[Follower],
    context: &str,
) {
    let (best, final_len) = {
        let chain = chain.lock().expect("lock");
        (chain.best.clone(), chain.final_height.map_or(0, |h| h + 1))
    };
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let converged = held.iter().zip(followers).all(|(held, follower)| {
            let held = held.lock().expect("lock");
            let lag = final_len.saturating_sub(held.durable.len());
            held.applied == best && lag <= follower.batch
        });
        if converged {
            return;
        }
        if running.producer.is_finished() {
            panic!("{context}: producer stopped: {:?}", (&mut running.producer).await);
        }
        for follower in &mut running.followers {
            if follower.is_finished() {
                panic!("{context}: follower stopped: {:?}", follower.await);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let held: Vec<String> = held
                .iter()
                .map(|held| {
                    let held = held.lock().expect("lock");
                    let diverged = held.applied.iter().zip(&best).position(|(a, b)| a != b);
                    let (durable, applied) = (held.durable.len(), held.applied.len());
                    format!("durable {durable} applied {applied} first off-best {diverged:?}")
                })
                .collect();
            panic!("{context}: never applied {} blocks: {held:?}", best.len());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn run(moves: Vec<Move>, followers: Vec<Follower>, lagging_at: usize, concurrency: usize) {
    let mut blocks = testing::Chain::new();
    let trunk = blocks.extend(blocks.genesis().hash, 5);
    let depth = ReorgDepth::new(NonZeroU32::new(DEPTH).expect("depth"));
    let mut headers = HeaderChain::regtest_in_memory(blocks.genesis().hash, depth);
    headers.insert_blocks(&blocks.path(trunk.hash)).expect("valid headers");
    if let Some(boundary) = headers.finalizable() {
        headers.finalize(boundary).expect("in-memory store");
    }
    let mut start_chain = Chain { headers, best: Vec::new(), final_height: None };
    start_chain.refresh();
    let genesis = start_chain.best.clone();
    let chain = Arc::new(Mutex::new(start_chain));
    let current = [Arc::new(MockChain::new()), Arc::new(MockChain::new())];
    let lagging = Arc::new(MockChain::new());
    let mut order = vec![Arc::clone(&current[0]), Arc::clone(&current[1])];
    order.insert(lagging_at, Arc::clone(&lagging));
    let mut validators = Validators { current, lagging, order, blocks, stalled: false };
    for mock in &validators.order {
        Validators::serve(mock, &genesis, &validators.blocks);
    }
    let held: Vec<Arc<Mutex<Held>>> = followers.iter().map(|_| Arc::default()).collect();

    let mut running = start(&validators, &chain, &held, &followers, concurrency);
    settle(&mut running, &chain, &held, &followers, "initial").await;
    let mut committed_ever: Vec<Vec<BlockHash>> = vec![Vec::new(); held.len()];
    for (step, change) in moves.iter().enumerate() {
        let context = format!("move {step} {change:?}");
        match change {
            Move::Chain { change, yields } => {
                validators.apply(&chain, change);
                running.verified.send_replace(Some(chain.lock().expect("lock").refresh()));
                match yields {
                    None => settle(&mut running, &chain, &held, &followers, &context).await,
                    Some(count) => {
                        for _ in 0..*count {
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }
            Move::Restart { down } => {
                stop(running, &context).await;
                for held in &held {
                    let mut held = held.lock().expect("lock");
                    held.applied = held.durable.clone();
                }
                for change in down {
                    validators.apply(&chain, change);
                }
                running = start(&validators, &chain, &held, &followers, concurrency);
                settle(&mut running, &chain, &held, &followers, &context).await;
            }
            Move::Lag { yields } => {
                validators.current[0].set_reachable(false);
                for _ in 0..*yields {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                validators.current[0].set_reachable(true);
                settle(&mut running, &chain, &held, &followers, &context).await;
            }
            Move::Stall(stalled) => validators.stalled = *stalled,
        }
        for (held, ever) in held.iter().zip(&mut committed_ever) {
            let held = held.lock().expect("lock");
            assert!(held.durable.starts_with(ever), "{context}: committed block rewritten");
            *ever = held.durable.clone();
        }
    }
    settle(&mut running, &chain, &held, &followers, "final").await;
    stop(running, "final").await;
}
