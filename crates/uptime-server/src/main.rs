use clap::Parser as _;
use tokio_util::sync::CancellationToken;
use uptime_server::{
    cli::{Cli, Command, SecretsAction},
    config::Config,
    serve, telemetry,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Cli::parse().command();
    if command
        == (Command::Secrets {
            action: SecretsAction::GenerateKey,
        })
    {
        // Needs no configuration: run it before loading any.
        return uptime_server::generate_key();
    }
    let config = Config::load()?;
    telemetry::init(&config.log)?;
    let shutdown = CancellationToken::new();
    serve::cancel_on_signal(shutdown.clone());
    let result = uptime_server::run(command, config, shutdown).await;
    if let Err(error) = &result {
        tracing::error!(error = format!("{error:#}"), "exiting with error");
    }
    result
}
