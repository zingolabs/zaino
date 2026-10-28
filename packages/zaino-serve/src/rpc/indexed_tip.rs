use std::pin::Pin;

use futures::{Stream, StreamExt as _};
use tokio::sync::watch;
use zaino_proto::proto::indexed_tip::{
    indexed_tip_service_server::IndexedTipService as IndexedTipServiceRpc, IndexedTip,
    SubscribeIndexedTipsRequest,
};
use zaino_state::IndexedTipIndexer;

pub(super) struct IndexedTipService<Indexer> {
    indexer: Indexer,
    shutdown: watch::Receiver<()>,
}

impl<Indexer> IndexedTipService<Indexer> {
    pub(super) const fn new(indexer: Indexer, shutdown: watch::Receiver<()>) -> Self {
        Self { indexer, shutdown }
    }
}

#[tonic::async_trait]
impl<Indexer: IndexedTipIndexer> IndexedTipServiceRpc for IndexedTipService<Indexer> {
    type SubscribeIndexedTipsStream =
        Pin<Box<dyn Stream<Item = Result<IndexedTip, tonic::Status>> + Send>>;

    async fn subscribe_indexed_tips(
        &self,
        _request: tonic::Request<SubscribeIndexedTipsRequest>,
    ) -> Result<tonic::Response<Self::SubscribeIndexedTipsStream>, tonic::Status> {
        let mut shutdown = self.shutdown.clone();
        let stream = self
            .indexer
            .subscribe_indexed_tips()
            .map(|tip| {
                Ok(IndexedTip {
                    height: u32::from(tip.height),
                    hash: <[u8; 32]>::from(tip.hash).to_vec(),
                })
            })
            .take_until(async move {
                let _ = shutdown.changed().await;
            });
        Ok(tonic::Response::new(Box::pin(stream)))
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, net::TcpListener, time::Duration};

    use futures::StreamExt as _;
    use tonic::service::Routes;
    use tonic::transport::{server::TcpIncoming, Server};
    use zaino_primitives::types::{BlockHash, BlockRef, Height};
    use zaino_proto::proto::indexed_tip::{
        indexed_tip_service_client::IndexedTipServiceClient,
        indexed_tip_service_server::IndexedTipServiceServer, SubscribeIndexedTipsRequest,
    };
    use zaino_state::IndexedTipIndexer;

    use super::IndexedTipService;
    use crate::server::{config::GrpcServerConfig, grpc::TonicServer};

    #[derive(Clone)]
    struct TestIndexer {
        updates: tokio::sync::watch::Receiver<BlockRef>,
    }

    impl IndexedTipIndexer for TestIndexer {
        fn subscribe_indexed_tips(&self) -> zaino_state::IndexedTipStream {
            let mut updates = self.updates.clone();
            let initial = *updates.borrow_and_update();
            let changes = futures::stream::unfold(updates, |mut updates| async move {
                updates.changed().await.ok()?;
                let tip = *updates.borrow_and_update();
                Some((tip, updates))
            });
            Box::pin(futures::stream::once(async move { initial }).chain(changes))
        }
    }

    fn tip(height: u32, hash_byte: u8) -> BlockRef {
        BlockRef {
            hash: BlockHash::from([hash_byte; 32]),
            height: Height::try_from(height).expect("test height is within the protocol limit"),
        }
    }

    #[tokio::test]
    async fn grpc_client_receives_initial_and_later_indexed_tips() -> Result<(), Box<dyn Error>> {
        // Given
        let (sender, receiver) = tokio::sync::watch::channel(tip(10, 1));
        let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(());
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let incoming = TcpIncoming::from(tokio::net::TcpListener::from_std(listener)?);
        let server = tokio::spawn(
            Server::builder()
                .add_service(IndexedTipServiceServer::new(IndexedTipService::new(
                    TestIndexer { updates: receiver },
                    shutdown,
                )))
                .serve_with_incoming(incoming),
        );
        let mut client = tokio::time::timeout(
            Duration::from_secs(5),
            IndexedTipServiceClient::connect(format!("http://{address}")),
        )
        .await??;

        // When
        let mut stream = client
            .subscribe_indexed_tips(SubscribeIndexedTipsRequest {})
            .await?
            .into_inner();
        let initial = stream.message().await?.expect("initial tip is present");
        sender.send_replace(tip(11, 2));
        let update = stream.message().await?.expect("updated tip is present");

        // Then
        assert_eq!((initial.height, initial.hash), (10, vec![1; 32]));
        assert_eq!((update.height, update.hash), (11, vec![2; 32]));

        server.abort();
        let _ = server.await;
        Ok(())
    }

    #[tokio::test]
    async fn active_subscription_ends_during_graceful_server_shutdown() -> Result<(), Box<dyn Error>>
    {
        // Given
        let (_sender, receiver) = tokio::sync::watch::channel(tip(10, 1));
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let mut server = TonicServer::spawn_from_listener_with_routes(
            |shutdown| {
                Routes::new(IndexedTipServiceServer::new(IndexedTipService::new(
                    TestIndexer { updates: receiver },
                    shutdown,
                )))
            },
            GrpcServerConfig {
                listen_address: address,
                tls: None,
            },
            listener,
        )
        .await?;
        let mut client = IndexedTipServiceClient::connect(format!("http://{address}")).await?;
        let mut stream = client
            .subscribe_indexed_tips(SubscribeIndexedTipsRequest {})
            .await?
            .into_inner();
        assert!(stream.message().await?.is_some());

        // When
        tokio::time::timeout(Duration::from_secs(1), server.close())
            .await
            .expect("graceful shutdown must not wait for an active indexed-tip stream");

        // Then
        assert!(stream.message().await?.is_none());
        Ok(())
    }
}
