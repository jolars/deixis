use std::error::Error;

use rmcp::{
    ServerHandler, ServiceExt,
    model::{Implementation, ServerCapabilities, ServerInfo},
    transport::stdio,
};
use tracing::info;

use crate::{
    config::Config,
    project::{Project, StartupState},
};

#[derive(Debug, Clone)]
pub struct DeixisServer {
    startup: StartupState,
}

impl DeixisServer {
    pub fn new(startup: StartupState) -> Self {
        Self { startup }
    }

    pub fn project(&self) -> &Project {
        self.startup.project()
    }

    pub fn config(&self) -> Option<&Config> {
        self.startup.config()
    }
}

impl ServerHandler for DeixisServer {
    fn get_info(&self) -> ServerInfo {
        let _project = self.project();
        ServerInfo::new(ServerCapabilities::default()).with_server_info(
            Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ),
        )
    }
}

pub async fn serve_stdio(startup: StartupState) -> Result<(), Box<dyn Error>> {
    let service = DeixisServer::new(startup).serve(stdio()).await?;
    let reason = service.waiting().await?;
    info!(?reason, "stopping deixis");

    Ok(())
}
