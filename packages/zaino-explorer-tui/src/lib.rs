//! TUI rendering and state, generic over the domain's [`ChainReader`] port.
//! The terminal lifecycle and event loop live only in `src/bin/tui.rs`; this
//! crate's logic (what actually gets tested) is state + a pure render
//! function over it.
#![forbid(unsafe_code)]

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;
use zaino_explorer_domain::{
    AddressSummary, AddressValidity, BlockDeltas, BlockDetail, BlockSummary, ChainReader,
    MempoolEntry, NodeDiagnostics, TransactionDetail, Treestate, UnifiedReceivers,
};

/// How many recent blocks the TUI lists.
const RECENT_BLOCKS: u32 = 10;

/// Which screen is showing. The home screen (height + recent blocks) is
/// always refreshed on a timer; the others are driven by user input — `t`
/// starts typing a txid, `a` starts typing an address, `b` starts typing a
/// block height or hash, `m` looks up the mempool directly (no input
/// needed), Enter looks up whatever's being typed, Esc returns home.
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
    /// A block's shielded commitment-tree state, looked up (via `s` on the
    /// `Block` screen) for the same height-or-hash.
    Treestate(String, Result<(), String>),
    /// The mempool's current contents, looked up via `m` on the home
    /// screen — no input needed, there's nothing to type.
    Mempool(Result<(), String>),
    /// Richer node diagnostics, looked up via `n` on the home screen — no
    /// input needed.
    NodeInfo(Result<(), String>),
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
    /// The looked-up address's validity — populated only on screen
    /// `Address`. Independent of `address` itself: worth showing even if
    /// the balance/txids read fails.
    address_validity: Option<Result<AddressValidity, String>>,
    /// The looked-up address's bundled receivers — populated only when
    /// `address_validity` says the address is unified.
    receivers: Option<Result<UnifiedReceivers, String>>,
    /// The looked-up block — populated only on screen `Block`.
    block: Option<BlockDetail>,
    /// The looked-up block's value movements — populated only on screen
    /// `Block`, and only when `getblockdeltas` succeeds; `None` renders as
    /// "unavailable" rather than hiding the rest of the block, since not
    /// every deployed zainod serves it.
    block_deltas: Option<BlockDeltas>,
    /// The looked-up block's shielded commitment-tree state — populated
    /// only on screen `Treestate`.
    treestate: Option<Treestate>,
    /// The looked-up mempool contents — populated only on screen `Mempool`.
    mempool: Vec<MempoolEntry>,
    /// The looked-up node diagnostics — populated only on screen
    /// `NodeInfo`.
    node_diagnostics: Option<NodeDiagnostics>,
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
        // Independent of the balance/txids/utxos/deltas read below —
        // worth showing even if that read fails.
        self.address_validity = Some(
            reader
                .validate_address(address.clone())
                .await
                .map_err(|e| e.to_string()),
        );
        // z_listunifiedreceivers rejects any non-unified address as a
        // parameter error, so only call it once validity has actually
        // said "unified".
        let is_unified =
            matches!(&self.address_validity, Some(Ok(v)) if v.kind.as_deref() == Some("unified"));
        self.receivers = if is_unified {
            Some(
                reader
                    .list_receivers(address.clone())
                    .await
                    .map_err(|e| e.to_string()),
            )
        } else {
            None
        };
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
                // `getblockdeltas` takes only a hash, and not every
                // deployed zainod serves it — a failure degrades to "no
                // movements shown" rather than hiding the block itself.
                self.block_deltas = reader.block_deltas(detail.hash.clone()).await.ok();
                self.block = Some(detail);
                self.screen = Screen::Block(id, Ok(()));
            }
            Err(e) => {
                self.block = None;
                self.block_deltas = None;
                self.screen = Screen::Block(id, Err(e.to_string()));
            }
        }
    }

    /// Look up the shielded commitment-tree state of the block currently
    /// shown on the `Block` screen. Does nothing if not currently on that
    /// screen.
    pub async fn lookup_treestate<C: ChainReader>(&mut self, reader: &C) {
        let Screen::Block(id, _) = &self.screen else {
            return;
        };
        let id = id.clone();
        match reader.treestate(id.clone()).await {
            Ok(state) => {
                self.treestate = Some(state);
                self.screen = Screen::Treestate(id, Ok(()));
            }
            Err(e) => {
                self.treestate = None;
                self.screen = Screen::Treestate(id, Err(e.to_string()));
            }
        }
    }

    /// Look up the mempool's current contents. Unlike every other lookup,
    /// there's nothing to type first — this can run directly from the
    /// home screen.
    pub async fn lookup_mempool<C: ChainReader>(&mut self, reader: &C) {
        match reader.raw_mempool().await {
            Ok(entries) => {
                self.mempool = entries;
                self.screen = Screen::Mempool(Ok(()));
            }
            Err(e) => {
                self.mempool = Vec::new();
                self.screen = Screen::Mempool(Err(e.to_string()));
            }
        }
    }

    /// Look up richer node diagnostics. Like the mempool, there's nothing
    /// to type first — this runs directly from the home screen.
    pub async fn lookup_node_diagnostics<C: ChainReader>(&mut self, reader: &C) {
        match reader.node_diagnostics().await {
            Ok(diagnostics) => {
                self.node_diagnostics = Some(diagnostics);
                self.screen = Screen::NodeInfo(Ok(()));
            }
            Err(e) => {
                self.node_diagnostics = None;
                self.screen = Screen::NodeInfo(Err(e.to_string()));
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
        Screen::Treestate(id, result) => render_treestate(frame, id, result, state),
        Screen::Mempool(result) => render_mempool(frame, result, state),
        Screen::NodeInfo(result) => render_node_info(frame, result, state),
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
            format!(
                "Chain height: {height}  (t: tx, a: address, b: block, m: mempool, n: node info, q: quit)"
            )
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

/// The validity line shown on the `Address` screen, independent of
/// whether the balance/txids read below it succeeded.
fn format_address_validity(validity: &Option<Result<AddressValidity, String>>) -> String {
    match validity {
        Some(Ok(v)) if v.valid => {
            let kind = v.kind.as_deref().unwrap_or("unknown kind");
            format!("Valid address — {kind}")
        }
        Some(Ok(_)) => "Not a recognized address on this network".to_string(),
        Some(Err(e)) => format!("Validity unavailable: {e}"),
        None => "Validity: not checked".to_string(),
    }
}

/// The receivers section, when a unified address's lookup populated it —
/// `None` when the address isn't unified, distinct from a failed lookup.
fn format_receivers(receivers: &Option<Result<UnifiedReceivers, String>>) -> Option<String> {
    match receivers {
        Some(Ok(r)) => {
            let mut lines = vec!["Receivers:".to_string()];
            if let Some(v) = &r.orchard {
                lines.push(format!("  Orchard: {v}"));
            }
            if let Some(v) = &r.sapling {
                lines.push(format!("  Sapling: {v}"));
            }
            if let Some(v) = &r.p2pkh {
                lines.push(format!("  Transparent (P2PKH): {v}"));
            }
            if let Some(v) = &r.p2sh {
                lines.push(format!("  Transparent (P2SH): {v}"));
            }
            Some(lines.join("\n"))
        }
        Some(Err(e)) => Some(format!("Receivers unavailable: {e}")),
        None => None,
    }
}

fn render_address(frame: &mut Frame, address: &str, result: &Result<(), String>, state: &AppState) {
    let text = match result {
        Err(e) => {
            let mut text = format!(
                "Address {address}\n\n{}",
                format_address_validity(&state.address_validity)
            );
            if let Some(receivers) = format_receivers(&state.receivers) {
                text.push('\n');
                text.push_str(&receivers);
            }
            text.push_str(&format!("\n\nRPC error: {e}\n\n(Esc: back)"));
            text
        }
        Ok(()) => {
            let Some(summary) = &state.address else {
                return;
            };
            let mut lines = vec![
                format!("Address {}", summary.address),
                format_address_validity(&state.address_validity),
            ];
            if let Some(receivers) = format_receivers(&state.receivers) {
                lines.push(receivers);
            }
            lines.push(format!("Balance: {} zat", summary.balance_zat));
            lines.push(format!("Lifetime received: {} zat", summary.received_zat));
            lines.push(String::new());
            lines.push("Transactions:".to_string());
            for txid in &summary.txids {
                lines.push(format!("  {txid}"));
            }
            lines.push(String::new());
            lines.push("Unspent outputs:".to_string());
            if summary.utxos.is_empty() {
                lines.push("  none (or unavailable on this deployment)".to_string());
            } else {
                for utxo in &summary.utxos {
                    lines.push(format!(
                        "  {}:{}  {} zat  height {}",
                        utxo.txid, utxo.output_index, utxo.value_zat, utxo.height
                    ));
                }
            }
            lines.push(String::new());
            lines.push("Value changes:".to_string());
            if summary.deltas.is_empty() {
                lines.push("  none (or unavailable on this deployment)".to_string());
            } else {
                for delta in &summary.deltas {
                    lines.push(format!(
                        "  {}  {} zat  height {}",
                        delta.txid, delta.value_zat, delta.height
                    ));
                }
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
            lines.push("Value movements:".to_string());
            match &state.block_deltas {
                Some(deltas) => {
                    for delta in &deltas.deltas {
                        lines.push(format!("  {}", delta.txid));
                        for movement in delta.inputs.iter().chain(delta.outputs.iter()) {
                            let address = movement
                                .address
                                .as_deref()
                                .map(|a| format!(" — {a}"))
                                .unwrap_or_default();
                            lines.push(format!("    {} zat{address}", movement.value_zat));
                        }
                    }
                }
                None => lines.push("  unavailable".to_string()),
            }
            lines.push(String::new());
            lines.push("(s: treestate, Esc: back)".to_string());
            lines.join("\n")
        }
    };
    frame.render_widget(
        Paragraph::new(text).block(Block::new().borders(Borders::ALL).title("Block")),
        frame.area(),
    );
}

fn render_treestate(frame: &mut Frame, id: &str, result: &Result<(), String>, state: &AppState) {
    let text = match result {
        Err(e) => format!("Treestate for block {id}\n\nRPC error: {e}\n\n(Esc: back)"),
        Ok(()) => {
            let Some(treestate) = &state.treestate else {
                return;
            };
            let mut lines = vec![
                format!("Treestate for block {}", treestate.height),
                format!("Hash: {}", treestate.hash),
                format!("Time: {}", treestate.time),
                String::new(),
            ];
            for (pool, tree) in [
                ("Sapling", &treestate.sapling),
                ("Orchard", &treestate.orchard),
                ("Ironwood", &treestate.ironwood),
            ] {
                lines.push(format!("{pool}:"));
                match tree {
                    Some(tree) => {
                        let root = tree.final_root.as_deref().unwrap_or("unavailable");
                        lines.push(format!("  Final root: {root}"));
                        lines.push(format!(
                            "  Final state: {}",
                            truncate_hex(&tree.final_state)
                        ));
                    }
                    None => lines.push("  not active at this block".to_string()),
                }
            }
            lines.push(String::new());
            lines.push("(Esc: back)".to_string());
            lines.join("\n")
        }
    };
    frame.render_widget(
        Paragraph::new(text).block(Block::new().borders(Borders::ALL).title("Treestate")),
        frame.area(),
    );
}

fn render_mempool(frame: &mut Frame, result: &Result<(), String>, state: &AppState) {
    let text = match result {
        Err(e) => format!("Mempool\n\nRPC error: {e}\n\n(Esc: back)"),
        Ok(()) => {
            let mut lines = vec![
                format!("{} transactions", state.mempool.len()),
                String::new(),
            ];
            for entry in &state.mempool {
                let time = entry
                    .time
                    .map(|t| format!(" — entered at {t}"))
                    .unwrap_or_default();
                lines.push(format!(
                    "{}  {} bytes  {} zat fee  height {}{time}",
                    entry.txid, entry.size, entry.fee_zat, entry.height
                ));
            }
            lines.push(String::new());
            lines.push("(Esc: back)".to_string());
            lines.join("\n")
        }
    };
    frame.render_widget(
        Paragraph::new(text).block(Block::new().borders(Borders::ALL).title("Mempool")),
        frame.area(),
    );
}

fn render_node_info(frame: &mut Frame, result: &Result<(), String>, state: &AppState) {
    let text = match result {
        Err(e) => format!("Node info\n\nRPC error: {e}\n\n(Esc: back)"),
        Ok(()) => {
            let Some(diagnostics) = &state.node_diagnostics else {
                return;
            };
            let mut lines = vec!["Mining:".to_string()];
            match &diagnostics.mining {
                Some(mining) => {
                    lines.push(format!("  Chain: {}", mining.chain));
                    if let Some(difficulty) = mining.difficulty {
                        lines.push(format!("  Difficulty: {difficulty}"));
                    }
                    if let Some(sol_ps) = mining.network_sol_ps {
                        lines.push(format!("  Network solution rate: {sol_ps} sol/s"));
                    }
                }
                None => lines.push("  unavailable on this deployment".to_string()),
            }
            lines.push(String::new());
            lines.push("Network:".to_string());
            match &diagnostics.network {
                Some(network) => {
                    lines.push(format!("  Protocol version: {}", network.protocol_version));
                    lines.push(format!("  Local services: {}", network.local_services));
                    lines.push(format!("  Relay fee: {} ZEC", network.relay_fee));
                    if !network.warnings.is_empty() {
                        lines.push(format!("  Warnings: {}", network.warnings));
                    }
                }
                None => lines.push("  unavailable on this deployment".to_string()),
            }
            lines.push(String::new());
            lines.push("Peers:".to_string());
            if diagnostics.peers.is_empty() {
                lines.push("  none".to_string());
            } else {
                for peer in &diagnostics.peers {
                    let direction = if peer.inbound { "inbound" } else { "outbound" };
                    lines.push(format!("  {}  {direction}", peer.addr));
                }
            }
            lines.push(String::new());
            lines.push("(Esc: back)".to_string());
            lines.join("\n")
        }
    };
    frame.render_widget(
        Paragraph::new(text).block(Block::new().borders(Borders::ALL).title("Node info")),
        frame.area(),
    );
}

/// Truncates a long hex string for display, noting its full length — a
/// serialized commitment tree can be large and isn't meant to be read raw.
fn truncate_hex(hex: &str) -> String {
    const PREVIEW_LEN: usize = 64;
    if hex.len() <= PREVIEW_LEN {
        return hex.to_string();
    }
    format!("{}… ({} hex chars total)", &hex[..PREVIEW_LEN], hex.len())
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
                utxos: Vec::new(),
                deltas: Vec::new(),
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
        assert!(
            content.contains("none (or unavailable on this deployment)"),
            "empty utxos/deltas should render as none, not blank: {content}"
        );
    }

    /// A looked-up address with scripted UTXOs and value changes renders
    /// both sections.
    #[test]
    fn renders_address_with_utxos_and_deltas() {
        use super::Screen;
        use zaino_explorer_domain::{AddressDelta, AddressSummary, AddressUtxo};

        let state = AppState {
            screen: Screen::Address("t1example".to_string(), Ok(())),
            address: Some(AddressSummary {
                address: "t1example".to_string(),
                balance_zat: 5_000,
                received_zat: 10_000,
                txids: vec!["ab".repeat(32)],
                utxos: vec![AddressUtxo {
                    txid: "cd".repeat(32),
                    output_index: 0,
                    script: "deadbeef".to_string(),
                    value_zat: 5_000,
                    height: 300,
                }],
                deltas: vec![AddressDelta {
                    txid: "cd".repeat(32),
                    index: 0,
                    height: 300,
                    value_zat: 5_000,
                }],
            }),
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
        assert!(content.contains(&"cd".repeat(32)), "{content}");
        assert!(content.contains("Unspent outputs"), "{content}");
        assert!(content.contains("Value changes"), "{content}");
    }

    /// A looked-up address's validity renders alongside its balance,
    /// independent of whether the balance read itself succeeded.
    #[test]
    fn renders_address_validity() {
        use super::Screen;
        use zaino_explorer_domain::{AddressSummary, AddressValidity};

        let state = AppState {
            screen: Screen::Address("t1example".to_string(), Ok(())),
            address: Some(AddressSummary {
                address: "t1example".to_string(),
                balance_zat: 0,
                received_zat: 0,
                txids: Vec::new(),
                utxos: Vec::new(),
                deltas: Vec::new(),
            }),
            address_validity: Some(Ok(AddressValidity {
                valid: true,
                address: Some("t1example".to_string()),
                kind: Some("p2pkh".to_string()),
            })),
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
        assert!(content.contains("Valid address"), "{content}");
        assert!(content.contains("p2pkh"), "{content}");
    }

    /// A looked-up unified address renders its bundled receivers.
    #[test]
    fn renders_address_receivers_for_a_unified_address() {
        use super::Screen;
        use zaino_explorer_domain::{AddressSummary, AddressValidity, UnifiedReceivers};

        let state = AppState {
            screen: Screen::Address("u1example".to_string(), Ok(())),
            address: Some(AddressSummary {
                address: "u1example".to_string(),
                balance_zat: 0,
                received_zat: 0,
                txids: Vec::new(),
                utxos: Vec::new(),
                deltas: Vec::new(),
            }),
            address_validity: Some(Ok(AddressValidity {
                valid: true,
                address: Some("u1example".to_string()),
                kind: Some("unified".to_string()),
            })),
            receivers: Some(Ok(UnifiedReceivers {
                orchard: Some("orchardreceiver".to_string()),
                sapling: None,
                p2pkh: Some("t1examplereceiver".to_string()),
                p2sh: None,
            })),
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
        assert!(content.contains("Receivers"), "{content}");
        assert!(content.contains("orchardreceiver"), "{content}");
        assert!(content.contains("t1examplereceiver"), "{content}");
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
        assert!(
            content.contains("unavailable"),
            "no block_deltas scripted should render as unavailable, not blank: {content}"
        );
    }

    /// A looked-up block with scripted value movements renders each
    /// transaction's signed inputs and outputs, with address.
    #[test]
    fn renders_block_with_value_movements() {
        use super::Screen;
        use zaino_explorer_domain::{BlockDeltas, BlockDetail, TransactionDelta, ValueMovement};

        let state = AppState {
            screen: Screen::Block("300".to_string(), Ok(())),
            block: Some(BlockDetail {
                height: 300,
                hash: "aa".repeat(32),
                time: 1_700_000_300,
                tx_ids: vec!["ab".repeat(32)],
            }),
            block_deltas: Some(BlockDeltas {
                hash: "aa".repeat(32),
                height: 300,
                deltas: vec![TransactionDelta {
                    txid: "ab".repeat(32),
                    inputs: vec![ValueMovement {
                        address: Some("t1spender".to_string()),
                        value_zat: -1_000,
                    }],
                    outputs: vec![ValueMovement {
                        address: Some("t1receiver".to_string()),
                        value_zat: 600,
                    }],
                }],
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
        assert!(content.contains("-1000 zat"), "{content}");
        assert!(content.contains("t1spender"), "{content}");
        assert!(content.contains("600 zat"), "{content}");
        assert!(content.contains("t1receiver"), "{content}");
    }

    /// A looked-up treestate renders each active pool's root and state,
    /// and an inactive pool's absence.
    #[test]
    fn renders_treestate_with_active_and_inactive_pools() {
        use super::Screen;
        use zaino_explorer_domain::{PoolTreestate, Treestate};

        let state = AppState {
            screen: Screen::Treestate("300".to_string(), Ok(())),
            treestate: Some(Treestate {
                hash: "aa".repeat(32),
                height: 300,
                time: 1_700_000_300,
                sapling: Some(PoolTreestate {
                    final_root: Some("cd".repeat(32)),
                    final_state: "deadbeef".to_string(),
                }),
                orchard: None,
                ironwood: None,
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
        assert!(content.contains(&"cd".repeat(32)), "{content}");
        assert!(content.contains("deadbeef"), "{content}");
        assert!(content.contains("not active at this block"), "{content}");
    }

    /// A failed treestate lookup renders the error, not a panic.
    #[test]
    fn renders_treestate_lookup_error() {
        use super::Screen;

        let state = AppState {
            screen: Screen::Treestate("999999".to_string(), Err("not found".to_string())),
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

    /// A looked-up mempool renders each entry's txid, size, fee, and
    /// height.
    #[test]
    fn renders_mempool_with_entries() {
        use super::Screen;
        use zaino_explorer_domain::MempoolEntry;

        let state = AppState {
            screen: Screen::Mempool(Ok(())),
            mempool: vec![MempoolEntry {
                txid: "ab".repeat(32),
                size: 250,
                fee_zat: 1_000,
                time: Some(1_700_000_300),
                height: 300,
            }],
            ..Default::default()
        };

        // Wide enough that the txid plus its trailing detail fits on one
        // line — a narrower backend truncates it, same as any terminal
        // too small for its content.
        let backend = TestBackend::new(140, 15);
        let mut terminal = Terminal::new(backend).expect("create terminal");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains(&"ab".repeat(32)), "{content}");
        assert!(content.contains("250 bytes"), "{content}");
        assert!(content.contains("1000 zat fee"), "{content}");
        assert!(content.contains("height 300"), "{content}");
    }

    /// An empty mempool renders zero transactions, not an error — an empty
    /// mempool is routine.
    #[test]
    fn renders_empty_mempool() {
        use super::Screen;

        let state = AppState {
            screen: Screen::Mempool(Ok(())),
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
        assert!(content.contains("0 transactions"), "{content}");
    }

    /// A failed mempool lookup renders the error, not a panic.
    #[test]
    fn renders_mempool_lookup_error() {
        use super::Screen;

        let state = AppState {
            screen: Screen::Mempool(Err("connection refused".to_string())),
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
        assert!(content.contains("connection refused"), "{content}");
    }

    /// A looked-up node-info screen renders mining/network facts and every
    /// peer.
    #[test]
    fn renders_node_info_with_peers() {
        use super::Screen;
        use zaino_explorer_domain::{MiningInfo, NetworkInfo, NodeDiagnostics, PeerInfo};

        let state = AppState {
            screen: Screen::NodeInfo(Ok(())),
            node_diagnostics: Some(NodeDiagnostics {
                mining: Some(MiningInfo {
                    chain: "main".to_string(),
                    difficulty: Some(42.5),
                    network_sol_ps: Some(1_000_000),
                }),
                network: Some(NetworkInfo {
                    protocol_version: 170_100,
                    local_services: "0000000000000000".to_string(),
                    relay_fee: 0.000_001,
                    warnings: String::new(),
                }),
                peers: vec![PeerInfo {
                    addr: "1.2.3.4:8233".to_string(),
                    inbound: true,
                }],
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
        assert!(content.contains("main"), "{content}");
        assert!(content.contains("42.5"), "{content}");
        assert!(content.contains("1.2.3.4:8233"), "{content}");
        assert!(content.contains("inbound"), "{content}");
    }

    /// A failed node-info lookup renders the error, not a panic.
    #[test]
    fn renders_node_info_lookup_error() {
        use super::Screen;

        let state = AppState {
            screen: Screen::NodeInfo(Err("connection refused".to_string())),
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
        assert!(content.contains("connection refused"), "{content}");
    }

    /// A hex string within the preview length renders in full; a longer
    /// one truncates and notes the full length rather than printing it raw.
    #[test]
    fn truncate_hex_leaves_short_strings_alone_and_truncates_long_ones() {
        assert_eq!(super::truncate_hex("deadbeef"), "deadbeef");
        let long = "ab".repeat(100);
        let truncated = super::truncate_hex(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.contains("200 hex chars total"));
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
                // "t1unknown" is not a well-formed address, so validity
                // reads as invalid — proof the validity pipe reached the
                // real adapter too, not just balance/txids.
                match &state.address_validity {
                    Some(Ok(v)) => assert!(!v.valid),
                    other => panic!("expected a populated validity result, got {other:?}"),
                }
            }
            other => panic!("expected a successful zero-balance Address screen, got {other:?}"),
        }

        let _ = handle.stop();
    }

    /// `lookup_address` against a real mock server for a real mainnet
    /// unified address: proof the `is_unified` gate actually fires and
    /// `list_receivers` reaches the real adapter, mirroring
    /// `zaino-explorer-web`'s equivalent coverage.
    #[tokio::test(flavor = "multi_thread")]
    async fn lookup_address_against_a_real_server_for_a_unified_address() {
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

        let ua = "u1pg2aaph7jp8rpf6yhsza25722sg5fcn3vaca6ze27hqjw7jvvhhuxkpcg0ge9xh6\
                  drsgdkda8qjq5chpehkcpxf87rnjryjqwymdheptpvnljqqrjqzjwkc2ma6hcq666k\
                  gwfytxwac8eyex6ndgr6ezte66706e3vaqrd25dzvzkc69kw0jgywtd0cmq52q5lkw\
                  6uh7hyvzjse8ksx";
        let mut state = AppState::default();
        state.start_address_input();
        for c in ua.chars() {
            state.push_char(c);
        }
        state.lookup_address(&reader).await;

        match &state.address_validity {
            Some(Ok(v)) => assert_eq!(v.kind.as_deref(), Some("unified")),
            other => panic!("expected a populated validity result, got {other:?}"),
        }
        match &state.receivers {
            Some(Ok(r)) => assert!(r.orchard.is_some()),
            other => panic!("expected populated receivers, got {other:?}"),
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
            CompactDifficulty, DecodedBlock, EquihashSolution, Height, Script, SignedZatoshis,
            TransactionId, Zatoshis,
        };
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zaino_service::{InputDelta, OutputDelta, TransactionDeltas};
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
        let p2pkh = |b: u8| {
            let mut bytes = vec![0x76, 0xa9, 0x14];
            bytes.extend_from_slice(&[b; 20]);
            bytes.extend_from_slice(&[0x88, 0xac]);
            Script::new(bytes)
        };
        let spend = TransactionDeltas {
            txid: TransactionId::from([0x7A; 32]),
            index: 0,
            inputs: vec![InputDelta {
                script: p2pkh(0x02),
                satoshis: SignedZatoshis::try_new(-1_000).expect("valid amount"),
                index: 0,
                prev_txid: TransactionId::from([0xAB; 32]),
                prevout: 2,
            }],
            outputs: vec![OutputDelta {
                script: p2pkh(0x03),
                satoshis: Zatoshis::new(600).expect("valid amount"),
                index: 0,
            }],
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
            block_deltas: Some(zaino_service::BlockDeltas {
                hash: BlockHash::from([0x11; 32]),
                confirmations: 1,
                size: 500,
                height: Height::try_from(300).expect("valid height"),
                version: 4,
                merkle_root: [0x22; 32].into(),
                deltas: vec![spend],
                time: 1_700_000_300,
                median_time: 1_700_000_000,
                nonce: [0x33; 32],
                bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
                difficulty: 1.0,
                chainwork: None,
                prev_hash: None,
                next_hash: None,
            }),
            treestate: Some(zaino_primitives::types::Treestate {
                block_hash: BlockHash::from([0x11; 32]),
                height: Height::try_from(300).expect("valid height"),
                time: 1_700_000_300,
                sapling: Some(zaino_primitives::types::PoolTreestate {
                    final_root: Some(zaino_primitives::types::TreeRoot::from([0x44; 32])),
                    final_state: vec![0xDE, 0xAD, 0xBE, 0xEF],
                }),
                orchard: None,
                ironwood: None,
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
                let deltas = state
                    .block_deltas
                    .as_ref()
                    .expect("block_deltas state populated");
                assert_eq!(deltas.deltas[0].inputs[0].value_zat, -1_000);
                assert_eq!(deltas.deltas[0].outputs[0].value_zat, 600);
            }
            other => panic!("expected a successful Block screen, got {other:?}"),
        }

        state.lookup_treestate(&reader).await;

        match &state.screen {
            Screen::Treestate(id, Ok(())) => {
                assert_eq!(id, "300");
                let treestate = state.treestate.as_ref().expect("treestate state populated");
                let sapling = treestate.sapling.as_ref().expect("sapling active");
                assert_eq!(sapling.final_state, "deadbeef");
            }
            other => panic!("expected a successful Treestate screen, got {other:?}"),
        }

        let _ = handle.stop();
    }

    /// `lookup_mempool` against a real mock server: proof the pipe reaches
    /// the real adapter, mirroring `zaino-explorer-web`'s equivalent
    /// coverage for the `/mempool` route.
    #[tokio::test(flavor = "multi_thread")]
    async fn lookup_mempool_against_a_real_server() {
        use super::Screen;
        use jsonrpsee::http_client::HttpClientBuilder;
        use std::net::TcpListener;
        use zaino_explorer_zaino_client::ZainoClient;
        use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
        use zaino_primitives::types::{BlockHash, BlockRef, Height, TransactionId};
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zaino_service::MempoolTx;
        use zcash_protocol::consensus::Network;

        let chain = MockChain {
            mempool: vec![MempoolTx {
                txid: TransactionId::from([0x7A; 32]),
                validated_against: BlockRef {
                    height: Height::try_from(300).expect("valid height"),
                    hash: BlockHash::from([0x11; 32]),
                },
            }],
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
        state.lookup_mempool(&reader).await;

        match &state.screen {
            Screen::Mempool(Ok(())) => {
                assert_eq!(state.mempool.len(), 1);
                assert_eq!(state.mempool[0].txid, "7a".repeat(32));
                assert_eq!(state.mempool[0].height, 300);
            }
            other => panic!("expected a successful Mempool screen, got {other:?}"),
        }

        let _ = handle.stop();
    }

    /// `lookup_node_diagnostics` against a real mock server: `getmininginfo`
    /// and `getnetworkinfo` can't be scripted on this mock (both always
    /// `NotReady`), so this proves the pipe reaches the real adapter and
    /// each degrades independently to a successful `NodeInfo` screen with
    /// absent sections, rather than failing the whole screen — mirroring
    /// `zaino-explorer-web`'s equivalent coverage for the `/node` route.
    #[tokio::test(flavor = "multi_thread")]
    async fn lookup_node_diagnostics_against_a_real_server() {
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
        state.lookup_node_diagnostics(&reader).await;

        match &state.screen {
            Screen::NodeInfo(Ok(())) => {
                let diagnostics = state
                    .node_diagnostics
                    .as_ref()
                    .expect("node_diagnostics state populated");
                assert!(diagnostics.mining.is_none());
                assert!(diagnostics.network.is_none());
            }
            other => panic!("expected a successful NodeInfo screen, got {other:?}"),
        }

        let _ = handle.stop();
    }
}
