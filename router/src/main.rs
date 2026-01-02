use clap::Parser;

use lpr_router::{
    config::RouterConfig, location::DocumentLocation, server, spec::CompiledSpec,
    template::render_env_template_with,
};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    config: Option<String>,
}

fn resolve_config_location(args: &Args) -> anyhow::Result<String> {
    if let Some(cfg) = &args.config {
        if !cfg.trim().is_empty() {
            return Ok(cfg.clone());
        }
    }

    for key in ["LPR_CONFIG_S3_URI", "LPR_CONFIG_URI"] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
    }

    anyhow::bail!(
        "missing config location: provide --config or set LPR_CONFIG_S3_URI/LPR_CONFIG_URI"
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let config_location = resolve_config_location(&args)?;
    tracing::info!(config = %config_location, "starting");

    let cfg_loc = DocumentLocation::parse(&config_location)?;
    let needs_s3 = matches!(cfg_loc, DocumentLocation::S3 { .. });
    let aws_cfg = if needs_s3 {
        Some(aws_config::from_env().load().await)
    } else {
        None
    };
    let s3 = aws_cfg.as_ref().map(aws_sdk_s3::Client::new);

    let cfg_bytes = cfg_loc.read_bytes(s3.as_ref()).await?;
    let cfg = RouterConfig::from_yaml_bytes(&cfg_bytes)?;

    let spec_loc = DocumentLocation::parse(&cfg.spec_path.to_string_lossy())?;
    let aws_cfg = if aws_cfg.is_none() && matches!(spec_loc, DocumentLocation::S3 { .. }) {
        Some(aws_config::from_env().load().await)
    } else {
        aws_cfg
    };
    let s3 = aws_cfg.as_ref().map(aws_sdk_s3::Client::new);

    let spec_bytes = spec_loc.read_bytes(s3.as_ref()).await?;
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
