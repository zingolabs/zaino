//! Producer + followers against validators whose best chain moves at random, checked against a
//! model of the best chain
//!
//! - Moves: extensions (some past the window), reorgs onto higher / equal / lower tips, bare
//!   retreats, bursts that never settle, restarts with the chain moving while down, quorum tips
//!   lagging onto an ancestor (in the window and under it)
//! - Three validators, 2-of-3 on the quorum tip, one serving the previous move's chain
//! - Every commit checked as it lands: final under the model's highest tip, on the best chain

use std::{
    num::{NonZeroU32, NonZeroUsize},
    sync::{Arc, Mutex},
    time::Duration,
};

use proptest::prelude::*;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use zaino_chainview::{EndpointSet, QuorumTip};
use zaino_primitives::types::{
    Block, BlockHash, BlockHeader, BlockRef, Height, ReorgDepth, Transaction,
};
use zaino_source::{mock::MockChain, BlockFetchPool, FetchRoute};
use zaino_sync::{BlockSink, ProduceError, Producer, Step, Subscription, Weight};

const DEPTH: u32 = 4;
const SETTLE: Duration = Duration::from_secs(10);

/// Validators' best chain + the highest tip it ever had (what bounds finality and legal forks)
#[derive(Debug)]
struct Chain {
    best: Vec<BlockHash>,
    highest: usize,
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
    /// from durable, on the applied branch where applied, final under the chain's highest tip, on
    /// its best chain
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
                assert!(at + DEPTH as usize <= chain.highest, "{at} committed before final");
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

/// Hash = (height, branch): every branch's block at a height is distinct
fn hash(height: u32, branch: u16) -> BlockHash {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&height.to_le_bytes());
    bytes[4..6].copy_from_slice(&branch.to_le_bytes());
    BlockHash::from(bytes)
}

fn block(height: u32, own: BlockHash, parent: BlockHash) -> Block {
    Block::new(
        BlockHeader::for_tests(height, own.into(), parent.into(), 0),
        vec![Transaction {
            txid: <[u8; 32]>::from(own).into(),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }],
    )
}

#[derive(Debug, Clone)]
enum Change {
    Extend(u32),
    /// Top `depth` blocks replaced by `len` on a new branch (`len` 0 = a retreat)
    Reorg {
        depth: u32,
        len: u32,
    },
}

#[derive(Debug, Clone)]
enum Move {
    /// `yields` = `None` → wait for every index to reach the new chain; `Some(n)` → `n` yields
    Chain { change: Change, yields: Option<u8> },
    /// Stop everything, move the chain while down, start again from what is durable
    Restart { down: Vec<Change> },
    /// Quorum tip `back` under the best tip for `yields` (agreers lagging), then back (chain fixed)
    Lag { back: u32, yields: u8 },
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
        3 => (1u32..=DEPTH, 0u32..=2 * DEPTH + 2)
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
            // past 2 × DEPTH = under the window floor
            2 => (1u32..=2 * DEPTH + 2, 0u8..=16).prop_map(|(back, yields)| Move::Lag { back, yields }),
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
    /// a block that is not final under the highest tip, or that is not on the best chain
    #[test]
    fn followers_track_the_best_chain_through_random_reorgs_and_restarts(
        moves in moves(),
        followers in followers(),
        lagging_at in 0usize..3,
        concurrency in 1usize..=4,
    ) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
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
    blocks: std::collections::HashMap<BlockHash, Block>,
    branch: u16,
}

impl Validators {
    fn serve(
        mock: &MockChain,
        best: &[BlockHash],
        blocks: &std::collections::HashMap<BlockHash, Block>,
    ) {
        mock.rewind_to(Height::try_from(0u32).expect("h"));
        mock.extend_best(best.iter().map(|own| blocks[own].clone()));
    }

    /// `change` applied to `chain` (fork parent clamped to `highest − DEPTH`: deeper = past
    /// finality, which halts); the lagging validator keeps the chain from before it
    fn apply(&mut self, chain: &Mutex<Chain>, change: &Change) {
        let mut chain = chain.lock().expect("chain lock");
        let before = chain.best.clone();
        let tip = before.len() - 1;
        let (keep, grow) = match *change {
            Change::Extend(count) => (before.len(), count),
            Change::Reorg { depth, len } => {
                self.branch += 1;
                let depth =
                    (depth as usize).min(tip - chain.highest.saturating_sub(DEPTH as usize));
                (before.len() - depth, len)
            }
        };
        chain.best.truncate(keep);
        for _ in 0..grow {
            let (height, parent) = (chain.best.len() as u32, *chain.best.last().expect("genesis"));
            let own = hash(height, self.branch);
            self.blocks.insert(own, block(height, own, parent));
            chain.best.push(own);
        }
        chain.highest = chain.highest.max(chain.best.len() - 1);
        for current in &self.current {
            Self::serve(current, &chain.best, &self.blocks);
        }
        Self::serve(&self.lagging, &before, &self.blocks);
    }
}

/// One running producer + its followers
struct Running {
    tips: watch::Sender<Option<QuorumTip>>,
    cancel: CancellationToken,
    producer: JoinHandle<Result<(), ProduceError>>,
    followers: Vec<JoinHandle<()>>,
}

/// `best`'s tip, agreed by the two current validators (the lagging one serves the last chain)
fn quorum(validators: &Validators, best: &[BlockHash]) -> Option<QuorumTip> {
    let current = |mock: &Arc<MockChain>| !Arc::ptr_eq(mock, &validators.lagging);
    Some(QuorumTip {
        block: BlockRef {
            hash: *best.last().expect("non-empty"),
            height: Height::try_from(best.len() as u32 - 1).expect("h"),
        },
        agreed_by: EndpointSet::at(
            (0..).zip(&validators.order).filter(|(_, mock)| current(mock)).map(|(at, _)| at),
        ),
    })
}

fn start(
    validators: &Validators,
    chain: &Arc<Mutex<Chain>>,
    held: &[Arc<Mutex<Held>>],
    followers: &[Follower],
    concurrency: usize,
) -> Running {
    let weight = block(0, hash(0, 0), hash(0, 0)).weight();
    let depth = ReorgDepth::new(NonZeroU32::new(DEPTH).expect("depth"));
    let mut block_sink = BlockSink::new("blocks");
    let (tips, tips_rx) = watch::channel(quorum(validators, &chain.lock().expect("lock").best));
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
    let pool = BlockFetchPool::new(
        validators.order.clone(),
        FetchRoute::Spread,
        NonZeroUsize::new(concurrency).expect("1..=4"),
    );
    let cancel = CancellationToken::new();
    let producer = Producer::new(block_sink, pool, tips_rx, depth, durable);
    let producer = tokio::spawn(producer.run(cancel.clone()));
    let followers =
        spawned.into_iter().map(|(recorder, blocks)| tokio::spawn(recorder.run(blocks))).collect();
    Running { tips, cancel, producer, followers }
}

async fn stop(running: Running, context: &str) {
    running.cancel.cancel();
    let produced = running.producer.await.expect("producer join");
    assert!(produced.is_ok(), "{context}: producer {produced:?}");
    for follower in running.followers {
        follower.await.unwrap_or_else(|panic| panic!("{context}: follower {panic:?}"));
    }
}

/// Every index applied the best chain, durable within the window + one batch of the tip
async fn settle(
    running: &mut Running,
    chain: &Mutex<Chain>,
    held: &[Arc<Mutex<Held>>],
    followers: &[Follower],
    context: &str,
) {
    let best = chain.lock().expect("lock").best.clone();
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let converged = held.iter().zip(followers).all(|(held, follower)| {
            let held = held.lock().expect("lock");
            let lag = best.len().saturating_sub(held.durable.len());
            held.applied == best && lag <= DEPTH as usize + follower.batch
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
            let held: Vec<_> = held.iter().map(|held| format!("{:?}", held.lock())).collect();
            panic!("{context}: never applied {} blocks: {held:?}", best.len());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn run(moves: Vec<Move>, followers: Vec<Follower>, lagging_at: usize, concurrency: usize) {
    let genesis: Vec<BlockHash> = (0..6).map(|height| hash(height, 0)).collect();
    let chain = Arc::new(Mutex::new(Chain { best: genesis.clone(), highest: genesis.len() - 1 }));
    let current = [Arc::new(MockChain::new()), Arc::new(MockChain::new())];
    let lagging = Arc::new(MockChain::new());
    let mut order = vec![Arc::clone(&current[0]), Arc::clone(&current[1])];
    order.insert(lagging_at, Arc::clone(&lagging));
    let mut validators =
        Validators { current, lagging, order, blocks: Default::default(), branch: 0 };
    for (height, own) in (0u32..).zip(&genesis) {
        let parent = height.checked_sub(1).map_or(hash(u32::MAX, 0), |up| hash(up, 0));
        validators.blocks.insert(*own, block(height, *own, parent));
    }
    for mock in &validators.order {
        Validators::serve(mock, &genesis, &validators.blocks);
    }
    let held: Vec<Arc<Mutex<Held>>> = followers.iter().map(|_| Arc::default()).collect();

    let mut running = start(&validators, &chain, &held, &followers, concurrency);
    settle(&mut running, &chain, &held, &followers, "initial bulk").await;
    let mut committed_ever: Vec<Vec<BlockHash>> = vec![Vec::new(); held.len()];
    for (step, change) in moves.iter().enumerate() {
        let context = format!("move {step} {change:?}");
        match change {
            Move::Chain { change, yields } => {
                validators.apply(&chain, change);
                running.tips.send_replace(quorum(&validators, &chain.lock().expect("lock").best));
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
            Move::Lag { back, yields } => {
                let best = chain.lock().expect("lock").best.clone();
                let back = (*back as usize).min(best.len() - 1);
                running.tips.send_replace(quorum(&validators, &best[..best.len() - back]));
                for _ in 0..*yields {
                    tokio::task::yield_now().await;
                }
                running.tips.send_replace(quorum(&validators, &best));
                settle(&mut running, &chain, &held, &followers, &context).await;
            }
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
