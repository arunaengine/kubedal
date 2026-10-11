use clap::{Arg, Command};
use kubedal::csi::{
    controller_server::ControllerServer, identity_server::IdentityServer, node_server::NodeServer,
};
use kubedal::node::kube::KubeCluster;
use kubedal::node::mounter::SystemMounter;
use kubedal::node::volume::{DEFAULT_CONTENT_CACHE_BYTES, VolumeManager};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::signal;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

use kubedal::services::{ControllerService, IdentityService, NodeService};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Default CSI driver name. It must match the name of the CSIDriver object.
const DEFAULT_NAME: &str = "kubedal.arunaengine.org";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // RUST_LOG controls the log level, for example "info" or "kubedal=debug".
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        // Colors only on a terminal, not in container logs.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_file(true)
        .with_line_number(true)
        .with_env_filter(filter)
        .init();

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
        .arg(
            Arg::new("data-dir")
                .long("data-dir")
                .help("Directory for volume data and state on the node")
                .default_value("/var/lib/kubelet/plugins/kubedal.arunaengine.org")
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("fuse-spool-size")
                .long("fuse-spool-size")
                .help("Space limit for FUSE files that are written but not uploaded yet, like 10Gi")
                .default_value("10Gi")
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("content-cache-size")
                .long("content-cache-size")
                .help("Size limit of the node content cache for file-list volumes, like 10Gi")
                .default_value("10Gi")
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("driver-name")
                .long("driver-name")
                .help("CSI driver name, must match the CSIDriver object")
                .default_value(DEFAULT_NAME)
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("storage-class")
                .long("storage-class")
                .help("StorageClass of the PVCs that DataReplicaSets create")
                .default_value("kubedal")
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("sync-image")
                .long("sync-image")
                .help("Image of the DataReplicaSet sync pods")
                .default_value(concat!(
                    "ghcr.io/arunaengine/kubedal:",
                    env!("CARGO_PKG_VERSION")
                ))
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("sync-image-pull-policy")
                .long("sync-image-pull-policy")
                .help("Image pull policy of the DataReplicaSet sync pods")
                .default_value("IfNotPresent")
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("sync-image-pull-secret")
                .long("sync-image-pull-secret")
                .help("Image pull secret of the DataReplicaSet sync pods (may be repeated)")
                .action(clap::ArgAction::Append)
                .value_parser(clap::value_parser!(String)),
        )
        .subcommand(
            Command::new("replicate")
                .about("Copy a DataReplicaSet source to its replicas (runs in the sync pod)"),
        )
        .get_matches();

    if matches.subcommand_matches("replicate").is_some() {
        return kubedal::replicate::run_from_env().map_err(Into::into);
    }

    // Both arguments have default values, so they are always present.
    let endpoint = matches
        .get_one::<String>("endpoint")
        .cloned()
        .unwrap_or_default();
    let node_id = matches
        .get_one::<String>("node-id")
        .cloned()
        .unwrap_or_default();
    let data_dir = matches
        .get_one::<PathBuf>("data-dir")
        .cloned()
        .unwrap_or_default();

    tracing::info!("Starting KubeDAL CSI Driver v{}", VERSION);
    tracing::info!("Endpoint: {}", endpoint);
    tracing::info!("Node ID: {}", node_id);

    let is_controller = matches!(node_id.as_str(), "controller");

    let client = kube::Client::try_default().await?;

    // Create service instances
    let driver_name = matches
        .get_one::<String>("driver-name")
        .cloned()
        .unwrap_or_else(|| DEFAULT_NAME.to_string());
    tracing::info!("Driver name: {}", driver_name);
    let identity_service = IdentityService::new(&driver_name, VERSION);

    // Parse the endpoint
    if !endpoint.starts_with("unix://") {
        return Err("Only unix domain sockets are supported".into());
    }

    let socket_path = endpoint.trim_start_matches("unix://").to_string();

    // Remove existing socket file if it exists
    let socket_path_obj = PathBuf::from(&socket_path);
    if socket_path_obj.exists() {
        std::fs::remove_file(&socket_path_obj)?;
    }

    // Ensure parent directory exists
    if let Some(parent) = socket_path_obj.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }

    // Create a Unix domain socket
    let uds = UnixListener::bind(&socket_path_obj)?;
    let uds_stream = UnixListenerStream::new(uds);

    tracing::info!("CSI server listening on {}", socket_path);

    // Build and start the gRPC server

    let builder = Server::builder().add_service(IdentityServer::new(identity_service));

    let builder = if is_controller {
        // Is a controller node
        let storage_class = matches
            .get_one::<String>("storage-class")
            .cloned()
            .unwrap_or_default();
        let sync_image = matches
            .get_one::<String>("sync-image")
            .cloned()
            .unwrap_or_default();
        let sync_pull_secrets = matches
            .get_many::<String>("sync-image-pull-secret")
            .unwrap_or_default()
            .cloned()
            .collect();
        let sync_pull_policy = matches
            .get_one::<String>("sync-image-pull-policy")
            .cloned()
            .unwrap_or_default();
        let controller_service = ControllerService::new(
            client.clone(),
            storage_class,
            sync_image,
            sync_pull_policy,
            sync_pull_secrets,
        )
        .await;
        builder.add_service(ControllerServer::new(controller_service))
    } else {
        // Is a csi driver noder
        let cluster = Arc::new(KubeCluster::new(client.clone(), &node_id));
        let spool_size = matches
            .get_one::<String>("fuse-spool-size")
            .map(|s| kubedal::util::quantity::parse_bytes(s))
            .transpose()
            .map_err(|e| format!("invalid --fuse-spool-size: {e}"))?
            .unwrap_or(u64::MAX);
        let spool_budget = Arc::new(kubedal::util::fuse_fs::SpoolBudget::new(spool_size));
        let mounter = Arc::new(SystemMounter::with_spool_budget(spool_budget.clone()));
        let cache_size = matches
            .get_one::<String>("content-cache-size")
            .map(|s| kubedal::util::quantity::parse_bytes(s))
            .transpose()
            .map_err(|e| format!("invalid --content-cache-size: {e}"))?
            .unwrap_or(DEFAULT_CONTENT_CACHE_BYTES);
        let manager = VolumeManager::new(&data_dir, cluster, mounter)?
            .with_content_cache_limit(cache_size)
            .with_spool_budget(spool_budget)?;
        tracing::info!("Data directory: {}", data_dir.display());
        // Clean up what a previous run of the plugin left behind.
        let report = manager.reconcile().await?;
        tracing::info!("Startup reconcile: {report:?}");
        let node_service = NodeService::new(client, &node_id, Arc::new(manager));
        builder.add_service(NodeServer::new(node_service))
    };

    builder
        .serve_with_incoming_shutdown(uds_stream, shutdown_signal())
        .await?;

    // Clean up the socket file
    if socket_path_obj.exists() {
        std::fs::remove_file(&socket_path_obj)?;
    }

    tracing::info!("CSI server stopped successfully");
    Ok(())
}

/// Wait for SIGTERM (sent by Kubernetes) or SIGINT (Ctrl+C).
async fn shutdown_signal() {
    let mut terminate = match signal::unix::signal(signal::unix::SignalKind::terminate()) {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::warn!("cannot listen for SIGTERM: {err}");
            None
        }
    };
    let terminate = async {
        match terminate.as_mut() {
            Some(stream) => {
                stream.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = signal::ctrl_c() => {}
        _ = terminate => {}
    }
    tracing::info!("Received shutdown signal, stopping CSI server...");
}
