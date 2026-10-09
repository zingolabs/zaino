//! Composition root for the TUI surface: terminal lifecycle, event loop, and
//! the concrete [`ZainoClient`] adapter — none of which the tested library
//! half (`zaino_explorer_tui::{AppState, render}`) knows about.
//!
//! Config: `ZAINO_RPC_URL` (default `http://127.0.0.1:8232`), same as the
//! web surface. `q` quits from anywhere. On the home screen: `t` starts a
//! txid lookup, `a` starts an address lookup, `b` starts a block
//! height/hash lookup, `m` looks up the mempool directly, `n` looks up
//! node diagnostics directly, and the chain height refreshes every 5s.
//! While typing: characters append, Backspace edits, Enter looks it up,
//! Esc cancels back home. On the block screen: `s` looks up its
//! treestate. On the transaction/address/block/treestate/mempool/
//! node-info screen: Esc goes back home.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use jsonrpsee::http_client::HttpClientBuilder;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use zaino_explorer_tui::{render, AppState, Screen};
use zaino_explorer_zaino_client::ZainoClient;

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    let rpc_url =
        std::env::var("ZAINO_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8232".to_string());
    let client = HttpClientBuilder::default()
        .build(&rpc_url)
        .unwrap_or_else(|e| panic!("invalid ZAINO_RPC_URL {rpc_url:?}: {e}"));
    let reader = ZainoClient::new(client);

    let mut terminal = setup_terminal().expect("set up terminal");
    let result = run(&mut terminal, &reader).await;
    teardown_terminal(&mut terminal).expect("restore terminal");
    result.expect("run loop");
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

fn teardown_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    Ok(())
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    reader: &ZainoClient,
) -> io::Result<()> {
    let mut state = AppState::default();
    state.refresh(reader).await;
    let mut last_refresh = tokio::time::Instant::now();

    loop {
        terminal.draw(|frame| render(frame, &state))?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q') {
                    return Ok(());
                }
                match state.screen() {
                    Screen::Home => match key.code {
                        KeyCode::Char('t') => state.start_txid_input(),
                        KeyCode::Char('a') => state.start_address_input(),
                        KeyCode::Char('b') => state.start_block_input(),
                        KeyCode::Char('m') => state.lookup_mempool(reader).await,
                        KeyCode::Char('n') => state.lookup_node_diagnostics(reader).await,
                        _ => {}
                    },
                    Screen::EnterTxid(_) => match key.code {
                        KeyCode::Char(c) => state.push_char(c),
                        KeyCode::Backspace => state.backspace(),
                        KeyCode::Enter => state.lookup_transaction(reader).await,
                        KeyCode::Esc => state.go_home(),
                        _ => {}
                    },
                    Screen::EnterAddress(_) => match key.code {
                        KeyCode::Char(c) => state.push_char(c),
                        KeyCode::Backspace => state.backspace(),
                        KeyCode::Enter => state.lookup_address(reader).await,
                        KeyCode::Esc => state.go_home(),
                        _ => {}
                    },
                    Screen::EnterBlock(_) => match key.code {
                        KeyCode::Char(c) => state.push_char(c),
                        KeyCode::Backspace => state.backspace(),
                        KeyCode::Enter => state.lookup_block(reader).await,
                        KeyCode::Esc => state.go_home(),
                        _ => {}
                    },
                    Screen::Block(_, _) => match key.code {
                        KeyCode::Char('s') => state.lookup_treestate(reader).await,
                        KeyCode::Esc => state.go_home(),
                        _ => {}
                    },
                    Screen::Transaction(_, _)
                    | Screen::Address(_, _)
                    | Screen::Treestate(_, _)
                    | Screen::Mempool(_)
                    | Screen::NodeInfo(_) => {
                        if key.code == KeyCode::Esc {
                            state.go_home();
                        }
                    }
                }
            }
        }

        if matches!(state.screen(), Screen::Home) && last_refresh.elapsed() >= REFRESH_INTERVAL {
            state.refresh(reader).await;
            last_refresh = tokio::time::Instant::now();
        }
    }
}
