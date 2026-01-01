use clap::Parser;

use lpr_router::{
    config::RouterConfig, server, spec::CompiledSpec, template::render_env_template_with,
};

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

    let cfg_bytes = tokio::fs::read(&args.config).await?;
    let cfg = RouterConfig::from_yaml_bytes(&cfg_bytes)?;

    let spec_bytes = tokio::fs::read(&cfg.spec_path).await?;
    let spec = match std::str::from_utf8(&spec_bytes) {
        Ok(s) => {
            let rendered = render_env_template_with(s, |k| std::env::var(k).ok())?;
            CompiledSpec::from_yaml_bytes(rendered.as_bytes(), cfg.default_timeout_ms)?
        }
        Err(_) => CompiledSpec::from_yaml_bytes(&spec_bytes, cfg.default_timeout_ms)?,
    };

    tracing::info!(listen_addr = %cfg.listen_addr, "loaded config + spec");
    server::run(cfg, spec).await
}
