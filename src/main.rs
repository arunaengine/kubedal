mod csi;
mod services;
mod resource;

use std::path::PathBuf;
use clap::{Arg, Command};
use csi::{controller_server::ControllerServer, identity_server::IdentityServer, node_server::NodeServer};
use tokio::net::UnixListener;
use tokio::signal;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

use services::{IdentityService, ControllerService, NodeService};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const NAME: &str = "kubedal.arunaengine.org";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Parse command line arguments
    let matches = Command::new("kubedal-csi")
        .version(VERSION)
        .about("KubeDAL CSI Driver for Kubernetes")
        .arg(
            Arg::new("endpoint")
                .short('e')
                .long("endpoint")
                .help("CSI endpoint")
                .default_value("unix:///tmp/csi.sock")
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("node-id")
                .short('n')
                .long("node-id")
                .help("Node ID")
                .default_value("node-1")
                .value_parser(clap::value_parser!(String)),
        )
        .get_matches();

    let endpoint = matches.get_one::<String>("endpoint").unwrap();
    let node_id = matches.get_one::<String>("node-id").unwrap();

    log::info!("Starting KubeDAL CSI Driver v{}", VERSION);
    log::info!("Endpoint: {}", endpoint);
    log::info!("Node ID: {}", node_id);

    // Create service instances
    let identity_service = IdentityService::new(NAME, VERSION);
    let controller_service = ControllerService::new();
    let node_service = NodeService::new(node_id);

    // Parse the endpoint
    if !endpoint.starts_with("unix://") {
        return Err("Only unix domain sockets are supported".into());
    }

    let socket_path = endpoint
        .trim_start_matches("unix://")
        .to_string();

    // Remove existing socket file if it exists
    let socket_path_obj = PathBuf::from(&socket_path);
    if socket_path_obj.exists() {
        std::fs::remove_file(&socket_path_obj)?;
    }

    // Ensure parent directory exists
    if let Some(parent) = socket_path_obj.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Create a Unix domain socket
    let uds = UnixListener::bind(&socket_path_obj)?;
    let uds_stream = UnixListenerStream::new(uds);

    log::info!("CSI server listening on {}", socket_path);

    // Build and start the gRPC server
    Server::builder()
        .add_service(IdentityServer::new(identity_service))
        .add_service(ControllerServer::new(controller_service))
        .add_service(NodeServer::new(node_service))
        .serve_with_incoming_shutdown(uds_stream, async {
            signal::ctrl_c().await.expect("Failed to listen for ctrl+c signal");
            log::info!("Received shutdown signal, stopping CSI server...");
        })
        .await?;

    // Clean up the socket file
    if socket_path_obj.exists() {
        std::fs::remove_file(&socket_path_obj)?;
    }

    log::info!("CSI server stopped successfully");
    Ok(())
}
