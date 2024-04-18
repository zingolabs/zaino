//! Utility functions for Zingo-RPC

#[macro_export]
macro_rules! define_grpc_passthrough {
    (fn
        $name:ident(
            &$self:ident$(,$($arg:ident: $argty:ty,)*)?
        ) -> $ret:ty
    ) => {
        #[must_use]
        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn $name<'life0, 'async_trait>(&'life0 $self$($(, $arg: $argty)*)?) ->
           ::core::pin::Pin<Box<
                dyn ::core::future::Future<
                    Output = ::core::result::Result<
                        ::tonic::Response<$ret>,
                        ::tonic::Status
                >
            > + ::core::marker::Send + 'async_trait
        >>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            println!("received call of {}", stringify!($name));
            Box::pin(async {
                ::zingo_netutils::GrpcConnector::new($self.lightwalletd_uri.clone())
                    .get_client()
                    .await
                    .expect("Proxy server failed to create client")
                    .$name($($($arg),*)?)
                    .await
            })
        }
    };
}
