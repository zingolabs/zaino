//! What one ChainHead publication costs.
//!
//! The chain head's per-tick work is dominated by the snapshot it builds: it
//! copies the published one, extends it, trims it, and publishes the result,
//! while readers keep earlier ones alive. This measures that path through the
//! service, so a change to how the snapshot is stored shows up here.
//!
//! The validator is a mock over the `zaino-source` ports, answering from
//! memory, so no I/O is measured. Blocks carry [`TRANSACTIONS`] transactions
//! each, because a snapshot copy copies what the blocks own.
//!
//! ```text
//! cargo bench -p zaino-chain-head-service --features testing
//! ```
//!
//! Criterion stores the results, so a later run reports the change against the
//! stored ones. To compare two commits: run it on the first with
//! `-- --save-baseline <name>`, then on the second with `-- --baseline <name>`.

use std::{
    collections::HashMap,
    num::{NonZeroU32, NonZeroU64},
    sync::{Arc, Mutex},
};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use zaino_chain_head::{ChainHeadBlockService as _, ChainHeadConfig};
use zaino_chain_head_service::{ChainHeadService, MapBackedSnapshot};
use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, EquihashSolution, Height,
    MerkleRoot, OrchardData, SaplingData, Transaction, TransactionId, TransparentData,
    TransparentInput, TreeRoots,
};
use zaino_source::{
    GetBlockByHashError, GetBlockError, GetChainTipError, GetCommitmentTreeRootsError,
    OneShotGetBlock, OneShotGetBlockByHash, OneShotGetChainTip, OneShotGetCommitmentTreeRoots,
    QueryError, SubscribeBlocks,
};

/// Blocks the chain head retains: the reorg limit plus its retention margin.
const WINDOW: u32 = 1_011;

/// Transactions per block. Mainnet blocks are usually smaller, but a snapshot
/// copy scales with this, and a busy block is the case that matters.
const TRANSACTIONS: usize = 32;

/// Transparent inputs per transaction, so a transaction owns heap data.
const INPUTS: usize = 2;

/// Publications measured per iteration, each with its snapshot still held.
const TICKS: u32 = 8;

/// A valid nBits value: non-negative, non-zero, no overflow.
const VALID_BITS: u32 = 0x2007_ffff;

// ------------------------------------------------------------- the validator

/// A validator answering from memory, with a chain the benchmark extends.
#[derive(Clone)]
struct MockValidator {
    state: Arc<Mutex<MockState>>,
}

struct MockState {
    blocks: HashMap<BlockHash, Block>,
    best_chain: Vec<BlockHash>,
}

impl MockValidator {
    /// A chain of `blocks` blocks, block `n` at height `n`.
    fn linear(blocks: u32) -> Self {
        let mut state = MockState {
            blocks: HashMap::new(),
            best_chain: Vec::new(),
        };
        for at in 0..blocks {
            let block = block_at(at);
            state.best_chain.push(block.header.hash);
            state.blocks.insert(block.header.hash, block);
        }
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().expect("mock state mutex poisoned")
    }

    /// Appends the block at the next height.
    fn extend(&self) {
        let mut state = self.lock();
        let at = u32::try_from(state.best_chain.len()).expect("bench heights fit u32");
        let block = block_at(at);
        state.best_chain.push(block.header.hash);
        state.blocks.insert(block.header.hash, block);
    }

    fn tip(&self) -> (BlockHash, Height) {
        let state = self.lock();
        let at = state.best_chain.len() - 1;
        (
            state.best_chain[at],
            height(u32::try_from(at).expect("bench heights fit u32")),
        )
    }
}

impl OneShotGetChainTip for MockValidator {
    async fn get_chain_tip(&self) -> Result<(BlockHash, Height), QueryError<GetChainTipError>> {
        Ok(self.tip())
    }
}

impl OneShotGetBlock for MockValidator {
    async fn get_block(&self, at: Height) -> Result<Block, QueryError<GetBlockError>> {
        let state = self.lock();
        state
            .best_chain
            .get(usize::try_from(u32::from(at)).expect("bench heights fit usize"))
            .and_then(|hash| state.blocks.get(hash))
            .cloned()
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(at)))
    }
}

impl OneShotGetBlockByHash for MockValidator {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.lock()
            .blocks
            .get(&hash)
            .cloned()
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl OneShotGetCommitmentTreeRoots for MockValidator {
    async fn get_commitment_tree_roots(
        &self,
        _block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        Ok(TreeRoots {
            sapling: None,
            orchard: None,
            ironwood: None,
        })
    }
}

impl SubscribeBlocks for MockValidator {}

// ---------------------------------------------------------------- the blocks

fn height(at: u32) -> Height {
    Height::try_from(at).expect("bench heights are in range")
}

fn hash(id: u32) -> BlockHash {
    let mut bytes = [0; 32];
    bytes[..4].copy_from_slice(&id.to_le_bytes());
    BlockHash::from(bytes)
}

/// The canonical block at `at`, carrying [`TRANSACTIONS`] transactions.
fn block_at(at: u32) -> Block {
    Block {
        header: BlockHeader {
            hash: hash(at),
            version: 4,
            prev_hash: hash(at.saturating_sub(1)),
            height: height(at),
            time: 0,
            merkle_root: MerkleRoot::from([0; 32]),
            block_commitments: BlockCommitments::from([0; 32]),
            bits: VALID_BITS,
            nonce: [0; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        },
        transactions: (0..TRANSACTIONS)
            .map(|index| transaction(at, index))
            .collect(),
        chain_metadata: ChainMetadata {
            sapling_tree_size: 0,
            orchard_tree_size: 0,
            ironwood_tree_size: 0,
        },
    }
}

fn transaction(at: u32, index: usize) -> Transaction {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&at.to_le_bytes());
    bytes[4..12].copy_from_slice(
        &u64::try_from(index)
            .expect("bench index fits u64")
            .to_le_bytes(),
    );
    let txid = TransactionId::from(bytes);
    Transaction {
        txid,
        transparent: TransparentData {
            inputs: (0..INPUTS)
                .map(|input| TransparentInput {
                    prev_txid: txid,
                    prev_index: u32::try_from(input).expect("input index fits u32"),
                })
                .collect(),
            outputs: Vec::new(),
        },
        sapling: SaplingData::default(),
        orchard: OrchardData::default(),
        ironwood: OrchardData::default(),
    }
}

// ------------------------------------------------------------- the scenarios

/// A config whose window matches [`WINDOW`], with the writer's timers pushed
/// out of the way: the benchmark steps the service itself.
fn config() -> ChainHeadConfig {
    let mut config =
        ChainHeadConfig::with_max_depth(NonZeroU32::new(WINDOW).expect("the window is not zero"));
    config.set_poll_interval_ms(NonZeroU64::new(3_600_000).expect("not zero"));
    config.set_initial_backoff_ms(NonZeroU64::new(1).expect("not zero"));
    config.set_max_backoff_ms(NonZeroU64::new(1).expect("not zero"));
    config.set_max_consecutive_failures(NonZeroU32::new(3).expect("not zero"));
    config
}

/// A chain head anchored against `validator`, with no writer task: the
/// benchmark advances it.
fn anchored(runtime: &Runtime, validator: &MockValidator) -> Arc<ChainHeadService<MockValidator>> {
    runtime.block_on(async {
        ChainHeadService::spawn_without_writer(
            Arc::new(validator.clone()),
            config(),
            CancellationToken::new(),
        )
        .await
        .expect("the mock validator is reachable")
    })
}

/// A chain head already holding the whole window.
fn synced(runtime: &Runtime, validator: &MockValidator) -> Arc<ChainHeadService<MockValidator>> {
    let service = anchored(runtime, validator);
    runtime.block_on(async { service.advance_once().await.expect("the advance succeeds") });
    service
}

fn publication(criterion: &mut Criterion) {
    let runtime = Runtime::new().expect("a tokio runtime");
    let mut group = criterion.benchmark_group("chain-head");
    group.sample_size(20);

    // Filling the window from the anchor: what a boot or a long catch-up does.
    group.bench_function("initial sync", |bencher| {
        bencher.iter_batched(
            || {
                let validator = MockValidator::linear(WINDOW);
                let service = anchored(&runtime, &validator);
                (validator, service)
            },
            |(_validator, service)| {
                runtime.block_on(async { service.advance_once().await.expect("advance succeeds") });
            },
            BatchSize::LargeInput,
        );
    });

    // Steady state: one new block per tick, with every published snapshot held,
    // as a reader holds them.
    group.bench_function("8 publications held", |bencher| {
        bencher.iter_batched(
            || {
                let validator = MockValidator::linear(WINDOW);
                let service = synced(&runtime, &validator);
                (validator, service)
            },
            |(validator, service)| {
                let mut held: Vec<Arc<MapBackedSnapshot>> =
                    Vec::with_capacity(usize::try_from(TICKS).expect("the tick count fits usize"));
                for _ in 0..TICKS {
                    validator.extend();
                    runtime.block_on(async {
                        service.advance_once().await.expect("advance succeeds")
                    });
                    held.push(service.subscriber().current());
                }
                held
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(benches, publication);
criterion_main!(benches);
