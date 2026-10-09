//! TUI rendering and state, generic over the domain's [`ChainReader`] port.
//! The terminal lifecycle and event loop live only in `src/bin/tui.rs`; this
//! crate's logic (what actually gets tested) is state + a pure render
//! function over it.
#![forbid(unsafe_code)]

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;
use zaino_explorer_domain::{
    AddressSummary, BlockDetail, BlockSummary, ChainReader, TransactionDetail,
};

/// How many recent blocks the TUI lists.
const RECENT_BLOCKS: u32 = 10;

/// Which screen is showing. The home screen (height + recent blocks) is
/// always refreshed on a timer; the others are driven by user input — `t`
/// starts typing a txid, `a` starts typing an address, `b` starts typing a
/// block height or hash, Enter looks it up, Esc returns home.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Home,
    /// Typing a txid to look up.
    EnterTxid(String),
    /// Looked up: the txid and what came back.
    Transaction(String, Result<(), String>),
    /// Typing an address to look up.
    EnterAddress(String),
    /// Looked up: the address and what came back.
    Address(String, Result<(), String>),
    /// Typing a block height or hash to look up.
    EnterBlock(String),
    /// Looked up: the height-or-hash and what came back.
    Block(String, Result<(), String>),
}

/// The TUI's whole state: the last successful read, or the last error, for
/// each of the things the current screen shows.
#[derive(Default, Clone)]
pub struct AppState {
    height: Option<u32>,
    blocks: Vec<BlockSummary>,
    error: Option<String>,
    screen: Screen,
    /// The looked-up transaction and its outputs' spend status, keyed by
    /// output index — populated only on screen `Transaction`.
    transaction: Option<TransactionDetail>,
    spends: Vec<Result<zaino_explorer_domain::SpendInfo, String>>,
    /// The looked-up address — populated only on screen `Address`.
    address: Option<AddressSummary>,
    /// The looked-up block — populated only on screen `Block`.
    block: Option<BlockDetail>,
    /// The validator/mempool status, refreshed alongside the home screen.
    node_status: Option<zaino_explorer_domain::NodeStatus>,
}

impl AppState {
    /// Refresh the home screen's state from a live read through `reader` —
    /// never a cached or locally re-derived value. Does nothing to the
    /// transaction screen's state; that's refreshed by `lookup_transaction`
    /// on demand, not on a timer.
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
        // Node status is shown separately and doesn't fold into `error` —
        // an unready node is routine (no validator behind a fresh deploy,
        // say), not the same class of failure as a height/block RPC error.
        self.node_status = reader.node_status().await.ok();
    }

    /// Enter txid-input mode, starting from an empty buffer.
    pub fn start_txid_input(&mut self) {
        self.screen = Screen::EnterTxid(String::new());
    }

    /// Enter address-input mode, starting from an empty buffer.
    pub fn start_address_input(&mut self) {
        self.screen = Screen::EnterAddress(String::new());
    }

    /// Enter block-input mode (height or hash), starting from an empty
    /// buffer.
    pub fn start_block_input(&mut self) {
        self.screen = Screen::EnterBlock(String::new());
    }

    /// Append a character to the current input buffer, if currently typing
    /// a txid, an address, or a block height/hash.
    pub fn push_char(&mut self, c: char) {
        match &mut self.screen {
            Screen::EnterTxid(buffer)
            | Screen::EnterAddress(buffer)
            | Screen::EnterBlock(buffer) => buffer.push(c),
            _ => {}
        }
    }

    /// Remove the last character from the current input buffer, if
    /// currently typing a txid, an address, or a block height/hash.
    pub fn backspace(&mut self) {
        match &mut self.screen {
            Screen::EnterTxid(buffer)
            | Screen::EnterAddress(buffer)
            | Screen::EnterBlock(buffer) => {
                buffer.pop();
            }
            _ => {}
        }
    }

    /// Return to the home screen from any other screen.
    pub fn go_home(&mut self) {
        self.screen = Screen::Home;
    }

    /// Which screen is currently active — the composition root's event loop
    /// reads this to decide how to interpret a key press.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Look up the txid currently in the input buffer: the transaction
    /// itself, then each output's spend status. Does nothing if not
    /// currently in input mode.
    pub async fn lookup_transaction<C: ChainReader>(&mut self, reader: &C) {
        let Screen::EnterTxid(txid) = &self.screen else {
            return;
        };
        let txid = txid.clone();
        match reader.transaction(txid.clone()).await {
            Ok(detail) => {
                let mut spends = Vec::with_capacity(detail.outputs.len());
                for index in 0..detail.outputs.len() {
                    spends.push(
                        reader
                            .spend_info(txid.clone(), index as u32)
                            .await
                            .map_err(|e| e.to_string()),
                    );
                }
                self.transaction = Some(detail);
                self.spends = spends;
                self.screen = Screen::Transaction(txid, Ok(()));
            }
            Err(e) => {
                self.transaction = None;
                self.spends = Vec::new();
                self.screen = Screen::Transaction(txid, Err(e.to_string()));
            }
        }
    }

    /// Look up the address currently in the input buffer. Does nothing if
    /// not currently in address-input mode.
    pub async fn lookup_address<C: ChainReader>(&mut self, reader: &C) {
        let Screen::EnterAddress(address) = &self.screen else {
            return;
        };
        let address = address.clone();
        match reader.address(address.clone()).await {
            Ok(summary) => {
                self.address = Some(summary);
                self.screen = Screen::Address(address, Ok(()));
            }
            Err(e) => {
                self.address = None;
                self.screen = Screen::Address(address, Err(e.to_string()));
            }
        }
    }

    /// Look up the block currently in the input buffer, by height or hash.
    /// Does nothing if not currently in block-input mode.
    pub async fn lookup_block<C: ChainReader>(&mut self, reader: &C) {
        let Screen::EnterBlock(id) = &self.screen else {
            return;
        };
        let id = id.clone();
        match reader.block(id.clone()).await {
            Ok(detail) => {
                self.block = Some(detail);
                self.screen = Screen::Block(id, Ok(()));
            }
            Err(e) => {
                self.block = None;
                self.screen = Screen::Block(id, Err(e.to_string()));
            }
        }
    }
}

/// Render the current state into `frame`, whichever screen is active.
pub fn render(frame: &mut Frame, state: &AppState) {
    match &state.screen {
        Screen::Home => render_home(frame, state),
        Screen::EnterTxid(buffer) => {
            render_input(frame, buffer, "Enter txid (Enter: look up, Esc: cancel)")
        }
        Screen::Transaction(txid, result) => render_transaction(frame, txid, result, state),
        Screen::EnterAddress(buffer) => {
            render_input(frame, buffer, "Enter address (Enter: look up, Esc: cancel)")
        }
        Screen::Address(address, result) => render_address(frame, address, result, state),
        Screen::EnterBlock(buffer) => render_input(
            frame,
            buffer,
            "Enter block height or hash (Enter: look up, Esc: cancel)",
        ),
        Screen::Block(id, result) => render_block(frame, id, result, state),
    }
}

fn render_home(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let [status_area, blocks_area] = Layout::new(
        Direction::Vertical,
        [Constraint::Length(4), Constraint::Min(0)],
    )
    .areas(area);

    let mut status_text = match (state.height, &state.error) {
        (Some(height), _) => {
            format!("Chain height: {height}  (t: tx, a: address, b: block, q: quit)")
        }
        (None, Some(err)) => format!("RPC error: {err}  (q to quit)"),
        (None, None) => "Loading...  (q to quit)".to_string(),
    };
    status_text.push('\n');
    status_text.push_str(&match &state.node_status {
        Some(status) => format!(
            "{} — {} peers — mempool: {} tx, {} bytes",
            status.subversion, status.connections, status.mempool_size, status.mempool_bytes
        ),
        None => "Node status unavailable".to_string(),
    });
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
        .map(|block| {
            ListItem::new(format!(
                "{}  {}  {}  {} tx",
                block.height, block.hash, block.time, block.tx_count
            ))
        })
        .collect();
    frame.render_widget(
        List::new(rows).block(Block::new().borders(Borders::ALL).title("Recent blocks")),
        blocks_area,
    );
}

fn render_input(frame: &mut Frame, buffer: &str, title: &str) {
    frame.render_widget(
        Paragraph::new(format!("{buffer}_")).block(Block::new().borders(Borders::ALL).title(title)),
        frame.area(),
    );
}

fn render_transaction(
    frame: &mut Frame,
    txid: &str,
    result: &Result<(), String>,
    state: &AppState,
) {
    let text = match result {
        Err(e) => format!("Transaction {txid}\n\nRPC error: {e}\n\n(Esc: back)"),
        Ok(()) => {
            let Some(tx) = &state.transaction else {
                return;
            };
            let mut lines = vec![
                format!("Transaction {}", tx.txid),
                format!("Size: {} bytes", tx.size),
            ];
            if let Some(height) = tx.height {
                lines.push(format!("Block height: {height}"));
            }
            if let Some(confirmations) = tx.confirmations {
                lines.push(format!("Confirmations: {confirmations}"));
            }
            lines.push(String::new());
            lines.push("Outputs:".to_string());
            for (output, spend) in tx.outputs.iter().zip(state.spends.iter()) {
                let addresses = if output.addresses.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", output.addresses.join(", "))
                };
                let spend_text = match spend {
                    Ok(s) => format!("spent by {} in block {}", s.spending_txid, s.height),
                    Err(_) => "unspent (or unknown)".to_string(),
                };
                lines.push(format!(
                    "  {} zat{addresses} — {spend_text}",
                    output.value_zat
                ));
            }
            lines.push(String::new());
            lines.push("(Esc: back)".to_string());
            lines.join("\n")
        }
    };
    frame.render_widget(
        Paragraph::new(text).block(Block::new().borders(Borders::ALL).title("Transaction")),
        frame.area(),
    );
}

fn render_address(frame: &mut Frame, address: &str, result: &Result<(), String>, state: &AppState) {
    let text = match result {
        Err(e) => format!("Address {address}\n\nRPC error: {e}\n\n(Esc: back)"),
        Ok(()) => {
            let Some(summary) = &state.address else {
                return;
            };
            let mut lines = vec![
                format!("Address {}", summary.address),
                format!("Balance: {} zat", summary.balance_zat),
                format!("Lifetime received: {} zat", summary.received_zat),
                String::new(),
                "Transactions:".to_string(),
            ];
            for txid in &summary.txids {
                lines.push(format!("  {txid}"));
            }
            lines.push(String::new());
            lines.push("(Esc: back)".to_string());
            lines.join("\n")
        }
    };
    frame.render_widget(
        Paragraph::new(text).block(Block::new().borders(Borders::ALL).title("Address")),
        frame.area(),
    );
}

fn render_block(frame: &mut Frame, id: &str, result: &Result<(), String>, state: &AppState) {
    let text = match result {
        Err(e) => format!("Block {id}\n\nRPC error: {e}\n\n(Esc: back)"),
        Ok(()) => {
            let Some(detail) = &state.block else {
                return;
            };
            let mut lines = vec![
                format!("Block {}", detail.height),
                format!("Hash: {}", detail.hash),
                format!("Time: {}", detail.time),
                String::new(),
                "Transactions:".to_string(),
            ];
            for txid in &detail.tx_ids {
                lines.push(format!("  {txid}"));
            }
            lines.push(String::new());
            lines.push("(Esc: back)".to_string());
            lines.join("\n")
        }
    };
    frame.render_widget(
        Paragraph::new(text).block(Block::new().borders(Borders::ALL).title("Block")),
        frame.area(),
    );
}

#[cfg(test)]
mod tests {
    use super::{render, AppState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use zaino_explorer_domain::{BlockSummary, TransactionDetail};

    /// A height in state renders into the frame buffer verbatim.
    #[test]
    fn renders_live_chain_height() {
        let state = AppState {
            height: Some(291),
            blocks: Vec::new(),
            error: None,
            ..Default::default()
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
            ..Default::default()
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
                    tx_count: 3,
                },
                BlockSummary {
                    height: 299,
                    hash: "bb".repeat(32),
                    time: 1_700_000_200,
                    tx_count: 1,
                },
            ],
            error: None,
            ..Default::default()
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

    /// The home screen's status bar renders node/mempool status alongside
    /// the height, independently — a `None` node status (not yet fetched,
    /// or the node isn't ready) renders a plain placeholder, not a panic.
    #[test]
    fn renders_node_status_alongside_height() {
        use zaino_explorer_domain::NodeStatus;

        let state = AppState {
            height: Some(300),
            node_status: Some(NodeStatus {
                subversion: "/Zebra:6.4.2/".to_string(),
                connections: 12,
                mempool_size: 3,
                mempool_bytes: 1_024,
            }),
            ..Default::default()
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
        assert!(content.contains("300"), "{content}");
        assert!(content.contains("/Zebra:6.4.2/"), "{content}");
        assert!(content.contains("12"), "{content}");
        assert!(content.contains("mempool: 3 tx"), "{content}");
    }

    /// Typing and editing a txid is pure state transition, no I/O: `t`
    /// starts input, characters append, backspace removes, Esc cancels back
    /// to the home screen.
    #[test]
    fn txid_input_mode_types_edits_and_cancels() {
        use super::Screen;

        let mut state = AppState::default();
        assert_eq!(state.screen, Screen::Home);

        state.start_txid_input();
        assert_eq!(state.screen, Screen::EnterTxid(String::new()));

        state.push_char('a');
        state.push_char('b');
        assert_eq!(state.screen, Screen::EnterTxid("ab".to_string()));

        state.backspace();
        assert_eq!(state.screen, Screen::EnterTxid("a".to_string()));

        state.go_home();
        assert_eq!(state.screen, Screen::Home);
    }

    /// The txid-input screen renders the typed buffer so far.
    #[test]
    fn renders_txid_input_buffer() {
        let mut state = AppState::default();
        state.start_txid_input();
        state.push_char('a');
        state.push_char('b');
        state.push_char('c');

        let backend = TestBackend::new(60, 10);
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
            content.contains("abc"),
            "buffer should contain the typed txid so far: {content}"
        );
    }

    /// A looked-up transaction renders its outputs, each with its spend
    /// status — "spent by X in block Y" or "unspent (or unknown)".
    #[test]
    fn renders_transaction_with_spend_status() {
        use super::Screen;
        use zaino_explorer_domain::{SpendInfo, TransactionOutput};

        let txid = "ab".repeat(32);
        let state = AppState {
            screen: Screen::Transaction(txid.clone(), Ok(())),
            transaction: Some(TransactionDetail {
                txid: txid.clone(),
                size: 250,
                height: Some(300),
                confirmations: Some(5),
                outputs: vec![
                    TransactionOutput {
                        value_zat: 1_000,
                        addresses: vec!["t1spent".to_string()],
                    },
                    TransactionOutput {
                        value_zat: 2_000,
                        addresses: vec!["t1unspent".to_string()],
                    },
                ],
            }),
            spends: vec![
                Ok(SpendInfo {
                    spending_txid: "cd".repeat(32),
                    spending_input_index: 0,
                    height: 301,
                }),
                Err("not found".to_string()),
            ],
            ..Default::default()
        };

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("t1spent"), "{content}");
        assert!(
            content.contains(&format!("spent by {}", "cd".repeat(32))),
            "{content}"
        );
        assert!(content.contains("t1unspent"), "{content}");
        assert!(content.contains("unspent (or unknown)"), "{content}");
    }

    /// A failed lookup renders the error, not a panic or a blank screen.
    #[test]
    fn renders_transaction_lookup_error() {
        use super::Screen;

        let state = AppState {
            screen: Screen::Transaction("ab".repeat(32), Err("no such transaction".to_string())),
            ..Default::default()
        };

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("no such transaction"), "{content}");
    }

    /// Typing and editing an address is pure state transition, mirroring
    /// the txid input mode.
    #[test]
    fn address_input_mode_types_and_cancels() {
        use super::Screen;

        let mut state = AppState::default();
        state.start_address_input();
        assert_eq!(state.screen, Screen::EnterAddress(String::new()));

        state.push_char('t');
        state.push_char('1');
        assert_eq!(state.screen, Screen::EnterAddress("t1".to_string()));

        state.backspace();
        assert_eq!(state.screen, Screen::EnterAddress("t".to_string()));

        state.go_home();
        assert_eq!(state.screen, Screen::Home);
    }

    /// A looked-up address renders its balance, lifetime received, and
    /// txids.
    #[test]
    fn renders_address_with_balance_and_txids() {
        use super::Screen;
        use zaino_explorer_domain::AddressSummary;

        let state = AppState {
            screen: Screen::Address("t1example".to_string(), Ok(())),
            address: Some(AddressSummary {
                address: "t1example".to_string(),
                balance_zat: 5_000,
                received_zat: 10_000,
                txids: vec!["ab".repeat(32)],
            }),
            ..Default::default()
        };

        let backend = TestBackend::new(80, 15);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("t1example"), "{content}");
        assert!(content.contains("5000"), "{content}");
        assert!(content.contains("10000"), "{content}");
        assert!(content.contains(&"ab".repeat(32)), "{content}");
    }

    /// Typing and editing a block height/hash is pure state transition,
    /// mirroring the txid/address input modes.
    #[test]
    fn block_input_mode_types_and_cancels() {
        use super::Screen;

        let mut state = AppState::default();
        state.start_block_input();
        assert_eq!(state.screen, Screen::EnterBlock(String::new()));

        state.push_char('3');
        state.push_char('0');
        state.push_char('0');
        assert_eq!(state.screen, Screen::EnterBlock("300".to_string()));

        state.backspace();
        assert_eq!(state.screen, Screen::EnterBlock("30".to_string()));

        state.go_home();
        assert_eq!(state.screen, Screen::Home);
    }

    /// A looked-up block renders its hash, time, and every txid.
    #[test]
    fn renders_block_with_hash_and_txids() {
        use super::Screen;
        use zaino_explorer_domain::BlockDetail;

        let state = AppState {
            screen: Screen::Block("300".to_string(), Ok(())),
            block: Some(BlockDetail {
                height: 300,
                hash: "aa".repeat(32),
                time: 1_700_000_300,
                tx_ids: vec!["ab".repeat(32)],
            }),
            ..Default::default()
        };

        let backend = TestBackend::new(80, 15);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("300"), "{content}");
        assert!(content.contains(&"aa".repeat(32)), "{content}");
        assert!(content.contains(&"ab".repeat(32)), "{content}");
    }

    /// A failed block lookup renders the error, not a panic.
    #[test]
    fn renders_block_lookup_error() {
        use super::Screen;

        let state = AppState {
            screen: Screen::Block("999999".to_string(), Err("not found".to_string())),
            ..Default::default()
        };

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("not found"), "{content}");
    }

    /// A failed address lookup renders the error, not a panic.
    #[test]
    fn renders_address_lookup_error() {
        use super::Screen;

        let state = AppState {
            screen: Screen::Address("t1bad".to_string(), Err("not found".to_string())),
            ..Default::default()
        };

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("not found"), "{content}");
    }

    /// `AppState::refresh` against a real adapter and a real server: proof
    /// the generic state logic actually works end-to-end with the one real
    /// `ChainReader`, not just with hand-constructed state.
    //
    // multi_thread required: drives a live jsonrpsee server (its own accept
    // loop) concurrently with the outbound RPC call, on one runtime — same
    // justification as zaino-explorer-web's identical test.
    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_populates_live_chain_height() {
        use jsonrpsee::http_client::HttpClientBuilder;
        use std::net::TcpListener;
        use zaino_explorer_zaino_client::ZainoClient;
        use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
        use zaino_primitives::types::{BlockHash, Height};
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let chain = MockChain {
            tip: Some(zaino_primitives::types::BlockRef {
                height: Height::try_from(291).expect("valid height"),
                hash: BlockHash::from([0xCDu8; 32]),
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

        let _ = handle.stop();
    }

    /// When the mock has no block data scripted at all (not even a tip),
    /// `refresh` reports the error rather than leaving stale or silently
    /// empty state — proof the error path is wired, not just the happy path.
    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_reports_error_when_the_node_has_nothing_scripted() {
        use jsonrpsee::http_client::HttpClientBuilder;
        use std::net::TcpListener;
        use zaino_explorer_zaino_client::ZainoClient;
        use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let handler = NodeRpc::new(
            MockIndexerService::new(MockChain::default()),
            Network::MainNetwork,
        );
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

        assert!(
            state.error.is_some(),
            "an unscripted node should surface as an error, not silence"
        );

        let _ = handle.stop();
    }

    /// `lookup_transaction` against a real mock server: proof the whole
    /// pipe (txid input -> transaction + per-output spend_info -> screen
    /// state) works end-to-end with the one real `ChainReader`, mirroring
    /// `zaino-explorer-web`'s equivalent coverage for the same capability.
    #[tokio::test(flavor = "multi_thread")]
    async fn lookup_transaction_against_a_real_server() {
        use super::Screen;
        use jsonrpsee::http_client::HttpClientBuilder;
        use std::net::TcpListener;
        use zaino_explorer_zaino_client::ZainoClient;
        use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let handler = NodeRpc::new(
            MockIndexerService::new(MockChain::default()),
            Network::MainNetwork,
        );
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
        state.start_txid_input();
        for c in "ab".repeat(32).chars() {
            state.push_char(c);
        }
        state.lookup_transaction(&reader).await;

        // Nothing is scripted on this mock, so the lookup is expected to
        // fail — this proves the pipe reaches the real adapter and reports
        // the failure on the Transaction screen, not that the mock has a
        // transaction fixture (zaino-noderpc's own fixtures for that are
        // substantial; the happy-path mapping is already covered by
        // zaino-explorer-zaino-client's pure-function tests).
        match &state.screen {
            Screen::Transaction(txid, Err(_)) => assert_eq!(txid, &"ab".repeat(32)),
            other => panic!("expected a failed Transaction screen, got {other:?}"),
        }

        let _ = handle.stop();
    }

    /// `lookup_address` against a real mock server: proof the pipe reaches
    /// the real adapter. An address with no scripted history and no
    /// scripted tip reads as a zero balance (explorer policy), not an
    /// error — mirroring `zaino-explorer-web`'s equivalent test and
    /// `zaino-explorer-zaino-client`'s own coverage of the happy path with
    /// a scripted balance.
    #[tokio::test(flavor = "multi_thread")]
    async fn lookup_address_against_a_real_server() {
        use super::Screen;
        use jsonrpsee::http_client::HttpClientBuilder;
        use std::net::TcpListener;
        use zaino_explorer_zaino_client::ZainoClient;
        use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let handler = NodeRpc::new(
            MockIndexerService::new(MockChain::default()),
            Network::MainNetwork,
        );
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
        state.start_address_input();
        for c in "t1unknown".chars() {
            state.push_char(c);
        }
        state.lookup_address(&reader).await;

        match &state.screen {
            Screen::Address(address, Ok(())) => {
                assert_eq!(address, "t1unknown");
                let summary = state.address.as_ref().expect("address state populated");
                assert_eq!(summary.balance_zat, 0);
            }
            other => panic!("expected a successful zero-balance Address screen, got {other:?}"),
        }

        let _ = handle.stop();
    }

    /// `lookup_block` against a real mock server: proof the pipe reaches
    /// the real adapter and populates `BlockDetail`, including txids,
    /// mirroring `zaino-explorer-web`'s equivalent coverage for the block
    /// route.
    #[tokio::test(flavor = "multi_thread")]
    async fn lookup_block_against_a_real_server() {
        use super::Screen;
        use jsonrpsee::http_client::HttpClientBuilder;
        use std::net::TcpListener;
        use zaino_explorer_zaino_client::ZainoClient;
        use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
        use zaino_primitives::types::{
            Block, BlockHash, BlockHeader, BlockRef, BlockTreeSizes, BlockVerbose, ChainMetadata,
            CompactDifficulty, DecodedBlock, EquihashSolution, Height,
        };
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let header = BlockHeader {
            hash: BlockHash::from([0x11; 32]),
            version: 4,
            prev_hash: BlockHash::from([0x22; 32]),
            height: Height::try_from(300).expect("valid height"),
            time: 1_700_000_300,
            merkle_root: [0x33; 32].into(),
            block_commitments: [0x44; 32].into(),
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
            nonce: [0x55; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        };
        let chain = MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(300).expect("valid height"),
                hash: BlockHash::from([0x11; 32]),
            }),
            block: Some(Block {
                header,
                transactions: Vec::new(),
                chain_metadata: ChainMetadata::ZERO,
            }),
            block_verbose: Some(BlockVerbose {
                confirmations: 1,
                difficulty: 1.0,
                chainwork: None,
                chain_supply: None,
                value_pools: Vec::new(),
                final_sapling_root: None,
                final_orchard_root: None,
                tree_sizes: BlockTreeSizes::default(),
                next_block_hash: None,
            }),
            decoded_block: Some(DecodedBlock {
                size: 1_000,
                transactions: Vec::new(),
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
        state.start_block_input();
        for c in "300".chars() {
            state.push_char(c);
        }
        state.lookup_block(&reader).await;

        match &state.screen {
            Screen::Block(id, Ok(())) => {
                assert_eq!(id, "300");
                let detail = state.block.as_ref().expect("block state populated");
                assert_eq!(detail.height, 300);
                assert_eq!(detail.hash, "11".repeat(32));
            }
            other => panic!("expected a successful Block screen, got {other:?}"),
        }

        let _ = handle.stop();
    }
}
