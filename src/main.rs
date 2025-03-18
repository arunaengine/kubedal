// use clap::{Arg, Command};
// use kubedal::csi::{
//     controller_server::ControllerServer, identity_server::IdentityServer, node_server::NodeServer,
// };
// use std::path::PathBuf;
// use tokio::net::UnixListener;
// use tokio::signal;
// use tokio_stream::wrappers::UnixListenerStream;
// use tonic::transport::Server;
// use tracing_subscriber::EnvFilter;

// use kubedal::services::{ControllerService, IdentityService, NodeService};

// const VERSION: &str = env!("CARGO_PKG_VERSION");
// const NAME: &str = "kubedal.arunaengine.org";

// #[tokio::main]
// async fn main() -> Result<(), Box<dyn std::error::Error>> {
//     let filter = EnvFilter::try_from_default_env()
//         .unwrap_or("none".into())
//         .add_directive("kubedal=trace".parse().unwrap());

//     tracing_subscriber::fmt()
//         .with_file(true)
//         .with_line_number(true)
//         .with_env_filter(filter)
//         .init();

//     // Parse command line arguments
//     let matches = Command::new("kubedal-csi")
//         .version(VERSION)
//         .about("KubeDAL CSI Driver for Kubernetes")
//         .arg(
//             Arg::new("endpoint")
//                 .short('e')
//                 .long("endpoint")
//                 .help("CSI endpoint")
//                 .default_value("unix:///tmp/csi.sock")
//                 .value_parser(clap::value_parser!(String)),
//         )
//         .arg(
//             Arg::new("node-id")
//                 .short('n')
//                 .long("node-id")
//                 .help("Node ID")
//                 .default_value("node-1")
//                 .value_parser(clap::value_parser!(String)),
//         )
//         .get_matches();

//     let endpoint = matches.get_one::<String>("endpoint").unwrap();
//     let node_id = matches.get_one::<String>("node-id").unwrap();

//     tracing::info!("Starting KubeDAL CSI Driver v{}", VERSION);
//     tracing::info!("Endpoint: {}", endpoint);
//     tracing::info!("Node ID: {}", node_id);

//     let is_controller = matches!(node_id.as_str(), "controller");

//     let client = kube::Client::try_default().await?;

//     // Create service instances
//     let identity_service = IdentityService::new(NAME, VERSION);

//     // Parse the endpoint
//     if !endpoint.starts_with("unix://") {
//         return Err("Only unix domain sockets are supported".into());
//     }

//     let socket_path = endpoint.trim_start_matches("unix://").to_string();

//     // Remove existing socket file if it exists
//     let socket_path_obj = PathBuf::from(&socket_path);
//     if socket_path_obj.exists() {
//         std::fs::remove_file(&socket_path_obj)?;
//     }

//     // Ensure parent directory exists
//     if let Some(parent) = socket_path_obj.parent() {
//         if !parent.exists() {
//             std::fs::create_dir_all(parent)?;
//         }
//     }

//     // Create a Unix domain socket
//     let uds = UnixListener::bind(&socket_path_obj)?;
//     let uds_stream = UnixListenerStream::new(uds);

//     tracing::info!("CSI server listening on {}", socket_path);

//     // Build and start the gRPC server

//     let builder = Server::builder().add_service(IdentityServer::new(identity_service));

//     let builder = if is_controller {
//         // Is a controller node
//         let controller_service = ControllerService::new(client.clone()).await;
//         builder.add_service(ControllerServer::new(controller_service))
//     } else {
//         // Is a csi driver noder
//         let node_service = NodeService::new(client, node_id);
//         builder.add_service(NodeServer::new(node_service))
//     };

//     builder
//         .serve_with_incoming_shutdown(uds_stream, async {
//             signal::ctrl_c()
//                 .await
//                 .expect("Failed to listen for ctrl+c signal");
//             tracing::info!("Received shutdown signal, stopping CSI server...");
//         })
//         .await?;

//     // Clean up the socket file
//     if socket_path_obj.exists() {
//         std::fs::remove_file(&socket_path_obj)?;
//     }

//     tracing::info!("CSI server stopped successfully");
//     Ok(())
// }

pub fn main() {}
