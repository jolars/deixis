use std::error::Error;

use rmcp::{
    ServerHandler, ServiceExt,
    model::{Implementation, ServerCapabilities, ServerInfo},
    transport::stdio,
};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, Default)]
struct DeixisServer;

impl ServerHandler for DeixisServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::default()).with_server_info(
            Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ),
        )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("deixis=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    info!(version = env!("CARGO_PKG_VERSION"), "starting deixis");
    let service = DeixisServer.serve(stdio()).await?;
    let reason = service.waiting().await?;
    info!(?reason, "stopping deixis");

    Ok(())
}
