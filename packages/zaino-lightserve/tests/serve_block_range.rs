//! End-to-end proof: a wallet-shaped `CompactTxStreamer` client streams real
//! compact blocks — composed on read from the index — over a real tonic server.
//!
//! Builds the current-zaino index set in-process over an in-memory backend from
//! shielded mock blocks (so the served `ChainMetadata` tree sizes are the
//! indexer's cumulative counts, not the source's), wraps it in the store's
//! compose-on-read `StoreReader`, stands up the real `zaino-lightserve`
//! `GrpcServer`, and drives `GetLatestBlock` / `GetBlockRange` / `GetBlock`
//! through a generated gRPC client over the wire. This is the finalised,
//! index-only serving slice — no validator passthrough, no runtime supervision.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;

use zaino_component::{CancellationToken, ReadySignal, Serve};
use zaino_indexer::{FullBlocks, SourceProvisioner};
use zaino_indexes::sets::current_zaino::{context_from_block, index_set};
use zaino_lightserve::{GrpcServer, LightServe};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{
    Block, CompactCiphertext, EphemeralKey, Height, NoteCommitment, Nullifier, OrchardAction,
    OrchardData, SaplingData, SaplingOutput, Transaction, TransactionId,
};
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zaino_proto::proto::service::{BlockId, BlockRange, ChainSpec};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::StoreReader;
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::primitives::BlockHeight;

/// One transaction committing `saplings` sapling outputs and `orchards` orchard
/// actions. Contents are filler — only the counts drive the tree sizes.
fn shielded_tx(seed: u8, saplings: usize, orchards: usize) -> Transaction {
    let sapling_output = || SaplingOutput {
        cmu: NoteCommitment::from([seed; 32]),
        ephemeral_key: EphemeralKey::from([seed; 32]),
        enc_ciphertext: CompactCiphertext::from([seed; CompactCiphertext::LENGTH]),
    };
    let orchard_action = || OrchardAction {
        nullifier: Nullifier::from([seed; 32]),
        cmx: NoteCommitment::from([seed; 32]),
        ephemeral_key: EphemeralKey::from([seed; 32]),
        enc_ciphertext: CompactCiphertext::from([seed; CompactCiphertext::LENGTH]),
    };
    Transaction {
        txid: TransactionId::from([seed; 32]),
        transparent: Default::default(),
        sapling: SaplingData {
            outputs: (0..saplings).map(|_| sapling_output()).collect(),
            ..Default::default()
        },
        orchard: OrchardData {
            actions: (0..orchards).map(|_| orchard_action()).collect(),
            ..Default::default()
        },
        ironwood: Default::default(),
    }
}

/// A mock block at `height` carrying one shielded transaction; the source
/// reports no tree sizes, so a non-zero served size can only come from indexing.
fn shielded_block(height: u32, hash_byte: u8, saplings: usize, orchards: usize) -> Block {
    let mut block = test_block(height, hash_byte);
    block.transactions = vec![shielded_tx(hash_byte, saplings, orchards)];
    block
}

/// Index `chain` [0, tip] into `backend` by driving the engine directly (the
/// provisioner feeds the engine's channel, bench-style) — no runtime harness.
async fn index_chain(backend: &InMemoryBackend, tip: u32) {
    let mut chain = MockChain::new();
    // Per-block commitments: h0: 2 sapling, 1 orchard; h1: 3 sapling, 0; h2: 0, 2.
    for (height, hash_byte, saplings, orchards) in
        [(0u32, 1u8, 2usize, 1usize), (1, 2, 3, 0), (2, 3, 0, 2)]
    {
        chain = chain.with_block(shielded_block(height, hash_byte, saplings, orchards));
    }
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    let mut engine = SyncEngine::from_index_set(
        index_set(),
        backend.clone(),
        EngineConfig {
            batch_size: 8,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("valid index set");

    let provisioner = Arc::new(SourceProvisioner::<_, _, _, FullBlocks>::new(
        source,
        |block| context_from_block(&block),
    ));
    let (tx, rx) = mpsc::channel(16);
    let feed = Arc::clone(&provisioner);
    let from = Height::try_from(0).expect("valid height");
    let to = Height::try_from(tip).expect("valid height");
    let provision = tokio::spawn(async move { feed.provision(from, to, tx).await });

    engine.sync_channel(rx).await.expect("sync the range");
    provision
        .await
        .expect("provision task joins")
        .expect("provision succeeds");
}

/// Stand up the real gRPC server over `store` on an ephemeral port; return its
/// address and a cancel handle. The server notifies readiness after it binds,
/// so the caller can connect without racing the bind.
async fn serve(store: StoreReader<InMemoryBackend>) -> (SocketAddr, CancellationToken) {
    // Discover a free port, then let the server rebind it.
    let addr: SocketAddr = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind probe socket")
        .local_addr()
        .expect("probe local addr");

    let server = Arc::new(GrpcServer::new(LightServe::new(store), addr));
    let cancel = CancellationToken::new();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let ready = ReadySignal::new(move || {
        let _ = ready_tx.send(());
    });
    let serve_cancel = cancel.clone();
    tokio::spawn(async move { server.serve(serve_cancel, ready).await });
    ready_rx.await.expect("server reports ready after binding");
    (addr, cancel)
}

/// A wallet-shaped client syncs real compact blocks — with the indexer's
/// cumulative tree sizes — from the index over the wire.
///
/// `multi_thread`: the tonic server runs in a spawned task and must make
/// progress concurrently with the client's calls on the test task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_streams_composed_compact_blocks_over_grpc() {
    let backend = InMemoryBackend::new();
    index_chain(&backend, 2).await;
    let store = StoreReader::new(Arc::new(backend));

    let (addr, cancel) = serve(store).await;
    let mut client = CompactTxStreamerClient::connect(format!("http://{addr}"))
        .await
        .expect("connect to the served port");

    // GetLatestBlock: the finalised tip, composed from the headers index.
    let tip = client
        .get_latest_block(ChainSpec {})
        .await
        .expect("get_latest_block")
        .into_inner();
    assert_eq!(tip.height, 2, "tip is the last indexed height");

    // GetBlockRange: the whole chain, streamed. Expected cumulative tree sizes
    // (sapling, orchard): h0 (2, 1), h1 (2+3=5, 1), h2 (5, 1+2=3).
    let range = BlockRange {
        start: Some(BlockId {
            height: 0,
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: 2,
            hash: Vec::new(),
        }),
        pool_types: Vec::new(),
    };
    let mut stream = client
        .get_block_range(range)
        .await
        .expect("get_block_range")
        .into_inner();

    let mut served = Vec::new();
    while let Some(block) = stream.message().await.expect("stream item") {
        served.push(block);
    }

    assert_eq!(served.len(), 3, "blocks 0..=2 streamed");
    let expected = [(0u64, 2u32, 1u32), (1, 5, 1), (2, 5, 3)];
    for (block, (height, sapling, orchard)) in served.iter().zip(expected) {
        assert_eq!(block.height, height, "block height in order");
        let meta = block
            .chain_metadata
            .as_ref()
            .expect("served block carries chain metadata");
        assert_eq!(
            meta.sapling_commitment_tree_size, sapling,
            "cumulative sapling tree size at height {height}"
        );
        assert_eq!(
            meta.orchard_commitment_tree_size, orchard,
            "cumulative orchard tree size at height {height}"
        );
    }

    // GetBlock: a single height composes to the same block the stream served.
    let single = client
        .get_block(BlockId {
            height: 1,
            hash: Vec::new(),
        })
        .await
        .expect("get_block")
        .into_inner();
    assert_eq!(single.height, 1);
    assert_eq!(
        single
            .chain_metadata
            .expect("chain metadata")
            .sapling_commitment_tree_size,
        5,
    );

    cancel.cancel();
}
