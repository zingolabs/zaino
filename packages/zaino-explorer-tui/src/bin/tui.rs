//! Composition root for the TUI surface: terminal lifecycle, event loop, and
//! the concrete [`ZainoClient`] adapter — none of which the tested library
//! half (`zaino_explorer_tui::{AppState, render}`) knows about.
//!
//! Config: `ZAINO_RPC_URL` (default `http://127.0.0.1:8232`), same as the
//! web surface. `q` or `Esc` quits; the chain height refreshes every 5s.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use jsonrpsee::http_client::HttpClientBuilder;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use zaino_explorer_tui::{render, AppState};
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
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    return Ok(());
                }
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            state.refresh(reader).await;
            last_refresh = tokio::time::Instant::now();
        }
    }
}
