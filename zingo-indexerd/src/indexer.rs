//! Zingo-Indexer server implementation.
//!
//! TODO: - Add ProxyServerError error type and rewrite functions to return <Result<(), ProxyServerError>>, propagating internal errors.
//!       - Update spawn_server and nym_spawn to return <Result<(), GrpcServerError>> and <Result<(), NymServerError>> and use here.

use crate::{nym_server::NymServer, server::spawn_grpc_server};
use zingo_rpc::{
    jsonrpc::connector::test_node_and_return_uri,
    proto::service::{compact_tx_streamer_client::CompactTxStreamerClient, Empty},
};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;

/// Launches test Zingo_Proxy server.
pub async fn spawn_indexer(
    indexer_port: &u16,
    lwd_port: &u16,
    zebrad_port: &u16,
    nym_conf_path: &str,
    online: Arc<AtomicBool>,
) -> (
    Vec<JoinHandle<Result<(), tonic::transport::Error>>>,
    Option<String>,
) {
    let mut handles = vec![];
    let nym_addr_out: Option<String>;

    startup_message();
    println!(
        "@zingoindexerd: Launching Zingo-Indexer!\n@zingoindexerd: Checking connection with node.."
    );
    // TODO Add user and password fields.
    let _zebrad_uri = test_node_and_return_uri(
        zebrad_port,
        Some("xxxxxx".to_string()),
        Some("xxxxxx".to_string()),
    )
    .await
    .unwrap();

    println!("@zingoindexerd: Launching gRPC Server..");
    let indexer_handle =
        spawn_grpc_server(indexer_port, lwd_port, zebrad_port, online.clone()).await;
    handles.push(indexer_handle);

    #[cfg(not(feature = "nym_poc"))]
    {
        wait_on_grpc_startup(indexer_port, online.clone()).await;
    }
    #[cfg(feature = "nym_poc")]
    {
        wait_on_grpc_startup(lwd_port, online.clone()).await;
    }

    #[cfg(not(feature = "nym_poc"))]
    {
        println!("@zingoindexerd[nym]: Launching Nym Server..");

        // let nym_server: NymServer = NymServer(NymClient::nym_spawn(nym_conf_path).await);
        // nym_addr_out = Some(nym_server.0 .0.nym_address().to_string());
        // let nym_indexer_handle = nym_server.serve(online).await;
        let nym_server = NymServer::new(nym_conf_path, online).await;
        nym_addr_out = Some(nym_server.nym_addr.clone());
        let nym_indexer_handle = nym_server.serve().await;

        handles.push(nym_indexer_handle);
        // TODO: Add wait_on_nym_startup(nym_addr_out, online.clone()) function to test nym server.
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }

    #[cfg(feature = "nym_poc")]
    {
        nym_addr_out = None;
    }
    (handles, nym_addr_out)
}

/// Closes test Zingo-Indexer servers currently active.
pub async fn close_indexer(online: Arc<AtomicBool>) {
    online.store(false, Ordering::SeqCst);
}

/// Tries to connect to the gRPC server and retruns if connection established. Shuts down with error message if connection with server cannot be established after 3 attempts.
async fn wait_on_grpc_startup(indexer_port: &u16, online: Arc<AtomicBool>) {
    let indexer_uri = http::Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{indexer_port}"))
        .path_and_query("/")
        .build()
        .unwrap();
    let mut attempts = 0;
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
    interval.tick().await;
    while attempts < 3 {
        match CompactTxStreamerClient::connect(indexer_uri.clone()).await {
            Ok(mut client) => match client.get_lightd_info(tonic::Request::new(Empty {})).await {
                Ok(_) => {
                    return;
                }
                Err(e) => {
                    println!(
                        "@zingoindexerd: GRPC server connection attempt {} failed with error: {}. Re",
                        attempts + 1,
                        e
                    );
                }
            },
            Err(e) => {
                println!(
                    "@zingoindexerd: GRPC server attempt {} failed to connect with error: {}",
                    attempts + 1,
                    e
                );
            }
        }
        attempts += 1;
        interval.tick().await;
    }
    println!("@zingoindexerd: Failed to start gRPC server, please check system config. Exiting Zingo-Indexer...");
    online.store(false, Ordering::SeqCst);
    std::process::exit(1);
}

fn startup_message() {
    let welcome_message = r#"
@@@@@@@@@@@@@@@&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&@@@@@@@@@
@@@@@@@@@@@@@&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&@@@@@@@
@@@@@@@@@&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&@@%(**/#@@@&&&&&&&&&&&&@@@
@@@@@@&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&@&.         /@&&&&&&&&&&&&&@
@@@&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&&&&&&&&@@            (@&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&@@.           (@&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&@@,    .    %@&&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&@@%@@#&@@&&&&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&@@&&&&&&&&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&%%&&&&&&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&#  %&%%%%&&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&(      /@%%%%&&&&&&&&&&&&
&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%@&#      %@@&%%%%&&&&&&&&&&
&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%#####################%%%%%%%%%%%%%%%%%%%%%&      %&%%%%%%%%&&&&&&&&
&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%##################################%%%%%%%%%%%%%%&&      %&%%%%%%%%%%&&&&&&
&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%###########################################%%%%%%%%@&#&      %&%%%%%%%%%%%&&&&&
&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%############################################%%&@@&%#(((((@      %&%%%%%%%%%%%%%&&&
&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%#############################%&@@@@@&%%##(((((((((((((((((((@      %&%%%%%%%%%%%%%%&&
&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%#%###%@@@@@&%########%&@&%#((((((((((((((((((((((((((((((((((((@      #&%%%%%%%%%%%%%%%&
&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%###%@#       .&@@@@@&(((((((((((((((((((((((((((((((((((((((((((#%%%@&&&%%%%%%%%%%%%%%%%%
&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%###@&          ,*@%((((((((((((((((((((((((((((((((((((((((((((((((((@%%%%%%%%%%%%%%%%%%%%
&&&%%%%%%%%%%%%%%%%%%%%%%%%%####%@/            %@(((((((((((((((((((((((((((((((((((((((((((((((((%&%%%%%%%%%%%%%%%%%%%%
&&&%%%%%%%%%%%%%%%%%%%%%%%%%#####&@.          *@#((((((((((((((((((((((((((((((((((((((((((((((((#@%%%%%%%%%%%%%%%%%%%%%
&&%%%%%%%%%%%%%%%%%%%%%%%%#######@@@&.      *@&((((((((((((((((((((((((((((((((((((((((((((((((((&%%%%%%%%%%%%%%%%%%%%%%
%&%%%%%%%%%%%%%%%%%%%%%%%%#####%@@%(((#&&&%#((((((((((((((((((((((((((((((((((((((((((((((((((((&%##%%%%%%%%%%%%%%%%%%%%
&%%%%%%%%%%%%%%%%%%%%%%%%#####%@(((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((&%####%%%%%%%%%%%%%%%%%%%
&%%%%%%%%%%%%%%%%%%%%%%%%####&%(((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((&%#####%%%%%%%%%%%%%%%%%%%
%%%%%%%%%%%%%%%%%%%%%%%%###%@((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((@%######%%%%%%%%%%%%%%%%%%%
%%%%%%%%%%%%%%%%%%%%%%%%##%@(((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((&&########%%%%%%%%%%%%%%%%%%%
&%%%%%%%%%%%%%%%%%%%%%%%#%@((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((#&##########%%%%%%%%%%%%%%%%%%%
&%%%%%%%%%%%%%%%%%%%%%%%%@(((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((#@############%%%%%%%%%%%%%%%%%%%
%%%%%%%%%%%%%%%%%%%%%%%%@#(((((((((((((((((((((((((((((((((((((((((((((((((((((((((((%@#############%%%%%%%%%%%%%%%%%%%%
&&%%%%%%%%%%%%%%%%%%%%%&&((((((((((((((((((((((((((((((((((((((((((((((((((((((((((&&################%%%%%%%%%%%%%%%%%%%
&&%%%%%%%%%%%%%%%%%%%%%@((((((((((((((((((((((((((((((((((((((((((((((((((((((((%@#################%%%%%%%%%%%%%%%%%%%%%
&&&%%%%%%%%%%%%%%%%%%%@%(((((((((((((((((((((((((((((((((((((((((((((((((((((%@%##################%%%%%%%%%%%%%%%%%%%%%%
&&&%%%%%%%%%%%%%%%%%%%@(((((((((((((((((((((((((((((((((((((((((((((((((((&&#####################%%%%%%%%%%%%%%%%%%%%%%%
&&&&&%%%%%%%%%%%%%%%%&%(((((((((((((((((((((((((((((((((((((((((((((((%@%######################%%%%%%%%%%%%%%%%%%%%%%%%%
&&&&&&%%%%%%%%%%%%%%%@#(((((((((((((((((((((((((((((((((((((((((((%@&#########################%%%%%%%%%%%%%%%%%%%%%%%%%%
&&&&&&%%%%%%%%%%%%%%%@(((((((((((((((((((((((((((((((((((((((#&@%###########################%%%%%%%%%%%%%%%%%%%%%%%%%%%&
&&&&&&&&%%%%%%%%%%%%&@((((((((((((((((((((((((((((((((((#&@&#############################%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&
&&&&&&&&&%%%%%%%%%%%&%((((((((((((((((((((((((((((#&@&%###############################%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&
&&&&&&&&&&%%%%%%%&@@@&@@@%((((((((((((((((((#&@&%%##################################%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&
&&&&&&&&&&&&%%&@(         #@#((((((((#&@&%%%%%%###############################%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&&&
&&&&&&&&&&&&&&@.           *@%&@@&%%%%%%%%%%%%%%%%%%%%%%%#%#%####%%##%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&&&&&
&&&&&&&&&&&&&&@            .@&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&&&&&&&
&&&&&&&&&&&&&&@&          .@@%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&@@#,   ,#@@%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&&&&&&&&&&&&
&&&&&&&&&&&&&&&&&&&&&&&&%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%&&&&&&&&&&&&&&&&&&
Thank you for using ZingoLabs ZingoIndexerD!
- Donate to us at https://free2z.cash/zingolabs.
- Submit any security conserns to us at zingodisclosure@proton.me.

****** Please note ZingoIndexerD is currently in development and should not be used to run mainnet nodes. ******

****** Currently LightwalletD is required for full functionality. ******
    "#;
    println!("{}", welcome_message);
}
