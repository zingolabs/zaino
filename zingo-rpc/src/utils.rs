//! Utility functions for Zingo-RPC.

/// Passes unimplemented RPCs on to Lightwalletd.
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
            println!("@zingoproxyd: Received call of {}.", stringify!($name));
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

/// Returns build info for Zingo-Proxy.
pub fn get_build_info() -> (String, String, String, String, String) {
    let commit_hash = env!("GIT_COMMIT").to_string();
    let branch = env!("BRANCH").to_string();
    let build_date = env!("BUILD_DATE").to_string();
    let build_user = env!("BUILD_USER").to_string();
    let version = env!("VERSION").to_string();
    (commit_hash, branch, build_date, build_user, version)
}
