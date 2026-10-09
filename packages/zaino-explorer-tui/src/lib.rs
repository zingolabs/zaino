//! TUI rendering and state, generic over the domain's [`ChainReader`] port.
//! The terminal lifecycle and event loop live only in `src/bin/tui.rs`; this
//! crate's logic (what actually gets tested) is state + a pure render
//! function over it.
#![forbid(unsafe_code)]

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;
use zaino_explorer_domain::{BlockSummary, ChainReader};

/// How many recent blocks the TUI lists.
const RECENT_BLOCKS: u32 = 10;

/// The TUI's whole state: the last successful read, or the last error, for
/// each of the two things this screen shows.
#[derive(Default, Clone)]
pub struct AppState {
    height: Option<u32>,
    blocks: Vec<BlockSummary>,
    error: Option<String>,
}

impl AppState {
    /// Refresh state from a live read through `reader` — never a cached or
    /// locally re-derived value.
    pub async fn refresh<C: ChainReader>(&mut self, reader: &C) {
        match reader.chain_height().await {
            Ok(height) => {
                self.height = Some(height);
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        match reader.recent_blocks(RECENT_BLOCKS).await {
            Ok(blocks) => self.blocks = blocks,
            Err(e) => self.error = Some(e.to_string()),
        }
    }
}

/// Render the current state into `frame`: a status panel on top, a recent-
/// blocks list below.
pub fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let [status_area, blocks_area] = Layout::new(
        Direction::Vertical,
        [Constraint::Length(3), Constraint::Min(0)],
    )
    .areas(area);

    let status_text = match (state.height, &state.error) {
        (Some(height), _) => format!("Chain height: {height}  (q to quit)"),
        (None, Some(err)) => format!("RPC error: {err}  (q to quit)"),
        (None, None) => "Loading...  (q to quit)".to_string(),
    };
    frame.render_widget(
        Paragraph::new(status_text).block(
            Block::new()
                .borders(Borders::ALL)
                .title("zaino-block-explorer"),
        ),
        status_area,
    );

    let rows: Vec<ListItem> = state
        .blocks
        .iter()
        .map(|block| ListItem::new(format!("{}  {}  {}", block.height, block.hash, block.time)))
        .collect();
    frame.render_widget(
        List::new(rows).block(Block::new().borders(Borders::ALL).title("Recent blocks")),
        blocks_area,
    );
}

#[cfg(test)]
mod tests {
    use super::{render, AppState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use zaino_explorer_domain::BlockSummary;

    /// A height in state renders into the frame buffer verbatim.
    #[test]
    fn renders_live_chain_height() {
        let state = AppState {
            height: Some(291),
            blocks: Vec::new(),
            error: None,
        };
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            content.contains("291"),
            "buffer should contain the height: {content}"
        );
    }

    /// An RPC error renders in place of a height, not silently dropped.
    #[test]
    fn renders_rpc_error_when_no_height_is_available() {
        let state = AppState {
            height: None,
            blocks: Vec::new(),
            error: Some("connection refused".to_string()),
        };
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            content.contains("connection refused"),
            "buffer should contain the error: {content}"
        );
    }

    /// The recent-blocks list renders each block's height into the buffer.
    #[test]
    fn renders_recent_blocks_list() {
        let state = AppState {
            height: Some(300),
            blocks: vec![
                BlockSummary {
                    height: 300,
                    hash: "aa".repeat(32),
                    time: 1_700_000_300,
                },
                BlockSummary {
                    height: 299,
                    hash: "bb".repeat(32),
                    time: 1_700_000_200,
                },
            ],
            error: None,
        };
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            content.contains(&"aa".repeat(32)),
            "buffer should contain the newer block's hash: {content}"
        );
        assert!(
            content.contains(&"bb".repeat(32)),
            "buffer should contain the older block's hash: {content}"
        );
        assert!(
            content.contains("Recent blocks"),
            "buffer should contain the list panel's title: {content}"
        );
    }

    /// `AppState::refresh` against a real adapter and a real server: proof
    /// the generic state logic actually works end-to-end with the one real
    /// `ChainReader`, not just with hand-constructed state.
    //
    // multi_thread required: drives a live jsonrpsee server (its own accept
    // loop) concurrently with the outbound RPC call, on one runtime — same
    // justification as zaino-explorer-web's identical test.
    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_populates_live_chain_height_and_blocks() {
        use jsonrpsee::http_client::HttpClientBuilder;
        use std::net::TcpListener;
        use zaino_explorer_zaino_client::ZainoClient;
        use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
        use zaino_primitives::types::rpc::BlockHeaderVerbose;
        use zaino_primitives::types::{BlockHash, CompactDifficulty, Height, MerkleRoot};
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zaino_service::BlockHashAt;
        use zcash_protocol::consensus::Network;

        let chain = MockChain {
            tip: Some(zaino_primitives::types::BlockRef {
                height: Height::try_from(291).expect("valid height"),
                hash: BlockHash::from([0xCDu8; 32]),
            }),
            block_hashes: (282..=291)
                .map(|h| BlockHashAt {
                    height: Height::try_from(h).expect("valid height"),
                    hash: BlockHash::from([0xCDu8; 32]),
                    time: 1_700_000_000,
                })
                .collect(),
            block_header_verbose: Some(BlockHeaderVerbose {
                hash: BlockHash::from([0xCDu8; 32]),
                confirmations: 1,
                height: Height::try_from(291).expect("valid height"),
                version: 4,
                merkle_root: MerkleRoot::from([0u8; 32]),
                time: 1_700_000_000,
                nonce: [0u8; 32],
                solution: Vec::new(),
                bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
                difficulty: 1.0,
                block_commitments: None,
                final_sapling_root: None,
                chainwork: None,
                previous_block_hash: None,
                next_block_hash: None,
            }),
            ..Default::default()
        };
        let handler = NodeRpc::new(MockIndexerService::new(chain), Network::MainNetwork);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.set_nonblocking(true).expect("set nonblocking");
        let addr = listener.local_addr().expect("local addr");
        let server = jsonrpsee::server::ServerBuilder::default()
            .build_from_tcp(listener)
            .expect("build server from listener");
        let handle = server.start(handler.into_rpc());

        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let mut state = AppState::default();
        state.refresh(&reader).await;

        assert_eq!(state.height, Some(291));
        assert_eq!(state.blocks.len(), 10);
        assert_eq!(state.blocks[0].height, 291, "newest first");
        assert!(state.error.is_none());

        let _ = handle.stop();
    }
}
