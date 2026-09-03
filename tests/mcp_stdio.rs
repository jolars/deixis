use std::{error::Error, time::Duration};

use rmcp::{
    ServiceExt, model::ServerCapabilities, transport::TokioChildProcess,
};
use tokio::{process::Command, time::timeout};

#[tokio::test]
async fn negotiates_an_empty_mcp_server_over_stdio()
-> Result<(), Box<dyn Error>> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_deixis"));
    command.env("RUST_LOG", "deixis=debug");
    let transport = TokioChildProcess::new(command)?;

    let client =
        timeout(Duration::from_secs(10), ().serve(transport)).await??;
    let peer = client
        .peer_info()
        .expect("server metadata should be retained");
    let implementation = peer
        .server_info
        .as_ref()
        .expect("an initialized server should identify itself");

    assert_eq!(implementation.name, "deixis");
    assert_eq!(implementation.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(peer.capabilities, ServerCapabilities::default());

    timeout(Duration::from_secs(10), client.cancel()).await??;
    Ok(())
}
