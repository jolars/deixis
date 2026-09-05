use std::error::Error;
use std::process::ExitCode;

use tracing::info;
use tracing_subscriber::EnvFilter;

use deixis::{cli::CliOptions, mcp, project::StartupState};

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("deixis: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let options = CliOptions::parse_env()?;
    let startup = StartupState::from_options(options)?;
    {
        let project = startup.project();
        let configured_servers =
            startup.config().map_or(0, |config| config.servers().len());
        let config_path =
            project.config_path().map(|path| path.display().to_string());
        info!(
            version = env!("CARGO_PKG_VERSION"),
            root = %project.root().display(),
            config = ?config_path,
            configured_servers,
            "starting deixis"
        );
    }

    mcp::serve_stdio(startup).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("deixis=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
