use anyhow::Context;
use clap::Parser;

use lpr_router::{config::RouterConfig, location::DocumentLocation, server, spec::CompiledSpec};

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

    if let Ok(v) = std::env::var("LPR_CONFIG_URI") {
        if !v.trim().is_empty() {
            return Ok(v);
        }
    }

    anyhow::bail!("missing config location: provide --config or set LPR_CONFIG_URI")
}

fn env_missing_or_empty(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => v.trim().is_empty(),
        Err(_) => true,
    }
}

fn configure_otel_env_defaults() {
    // Export traces to the local collector by default (for App Runner X-Ray integration).
    if env_missing_or_empty("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        && env_missing_or_empty("OTEL_EXPORTER_OTLP_ENDPOINT")
    {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4317");
    }

    // Accept AWS X-Ray trace headers (`X-Amzn-Trace-Id`) and W3C trace context.
    if env_missing_or_empty("OTEL_PROPAGATORS") {
        std::env::set_var("OTEL_PROPAGATORS", "xray,tracecontext,baggage");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    configure_otel_env_defaults();

    let _otel_guard = init_tracing_opentelemetry::TracingConfig::production()
        .with_file_names(false)
        .with_resource_config(
            init_tracing_opentelemetry::resource::DetectResource::default()
                .with_fallback_service_name(env!("CARGO_PKG_NAME"))
                .with_fallback_service_version(env!("CARGO_PKG_VERSION")),
        )
        .init_subscriber()
        .context("init tracing")?;

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
    let mut cfg = RouterConfig::from_yaml_bytes(&cfg_bytes)?;

    let spec_doc = cfg.spec.take().context("router config is missing `Spec`")?;
    let spec = CompiledSpec::from_spec(spec_doc, cfg.default_timeout_ms)?;

    tracing::info!(listen_addr = %cfg.listen_addr, "loaded config + spec");
    server::run(cfg, spec).await
}
