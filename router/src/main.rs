use clap::Parser;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    tracing::info!(config = %args.config, "starting");

    // Scaffold only; implementation lands in follow-up commits.
    let _cfg_bytes = tokio::fs::read(args.config).await?;
    Ok(())
}

