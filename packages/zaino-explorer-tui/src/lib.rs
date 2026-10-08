//! TUI rendering and state, generic over the domain's [`ChainReader`] port.
//! The terminal lifecycle and event loop live only in `src/bin/tui.rs`; this
//! crate's logic (what actually gets tested) is state + a pure render
//! function over it.
#![forbid(unsafe_code)]

use ratatui::widgets::Paragraph;
use ratatui::Frame;
use zaino_explorer_domain::ChainReader;

/// The TUI's whole state: the last successful height, or the last error.
#[derive(Default, Clone)]
pub struct AppState {
    height: Option<u32>,
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
    }
}

/// Render the current state into `frame`.
pub fn render(frame: &mut Frame, state: &AppState) {
    let text = match (state.height, &state.error) {
        (Some(height), _) => format!("zaino-block-explorer\n\nChain height: {height}\n\nq: quit"),
        (None, Some(err)) => format!("zaino-block-explorer\n\nRPC error: {err}\n\nq: quit"),
        (None, None) => "zaino-block-explorer\n\nLoading...\n\nq: quit".to_string(),
    };
    frame.render_widget(Paragraph::new(text), frame.area());
}

#[cfg(test)]
mod tests {
    use super::{render, AppState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// A height in state renders into the frame buffer verbatim.
    #[test]
    fn renders_live_chain_height() {
        let state = AppState {
            height: Some(291),
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
        use zaino_service::testing::{MockChain, MockIndexerService};
        use zcash_protocol::consensus::Network;

        let chain = MockChain {
            tip: Some(zaino_primitives::types::BlockRef {
                height: zaino_primitives::types::Height::try_from(291).expect("valid height"),
                hash: zaino_primitives::types::BlockHash::from([0xCDu8; 32]),
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
        assert!(state.error.is_none());

        let _ = handle.stop();
    }
}
