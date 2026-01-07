use anyhow::Context;
use clap::Parser;
use opentelemetry::trace::TracerProvider as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

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

struct TracerProviderGuard(opentelemetry_sdk::trace::SdkTracerProvider);

impl Drop for TracerProviderGuard {
    fn drop(&mut self) {
        let _ = self.0.force_flush();
        let _ = self.0.shutdown();
    }
}

fn otlp_traces_endpoint_env() -> Option<String> {
    [
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
    ]
    .into_iter()
    .find_map(|name| std::env::var(name).ok())
    .map(|v| v.trim().to_string())
    .filter(|v| !v.is_empty())
}

fn init_logging_and_tracing() -> anyhow::Result<(bool, Option<TracerProviderGuard>)> {
    let otel_enabled = otlp_traces_endpoint_env().is_some();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let fmt = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_file(false)
        .with_line_number(false)
        .with_target(true)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339());

    if !otel_enabled {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt)
            .try_init()
            .context("init tracing subscriber")?;
        return Ok((false, None));
    }

    // `axum-tracing-opentelemetry` creates spans with target `otel::tracing` at `trace` level.
    // Ensure those spans are enabled even when the app logs at `info` by default.
    let filter = match std::env::var("RUST_LOG") {
        Ok(v) if v.contains("otel::tracing") => filter,
        _ => filter.add_directive("otel::tracing=trace".parse()?),
    };

    // If OTEL is enabled, provide sane defaults for App Runner X-Ray integration. The exporter endpoint
    // must be configured explicitly via env vars (for example `OTEL_EXPORTER_OTLP_ENDPOINT`).
    if env_missing_or_empty("OTEL_PROPAGATORS") {
        std::env::set_var("OTEL_PROPAGATORS", "xray,tracecontext,baggage");
    }
    if env_missing_or_empty("OTEL_METRICS_EXPORTER") {
        std::env::set_var("OTEL_METRICS_EXPORTER", "none");
    }

    let resource = init_tracing_opentelemetry::resource::DetectResource::default()
        .with_fallback_service_name(env!("CARGO_PKG_NAME"))
        .with_fallback_service_version(env!("CARGO_PKG_VERSION"))
        .build();

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .context("build OTLP span exporter")?;

    let batch =
        opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor::builder(
            exporter,
            opentelemetry_sdk::runtime::Tokio,
        )
        .build();

    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(batch)
        .with_id_generator(opentelemetry_aws::trace::XrayIdGenerator::default())
        .build();

    init_tracing_opentelemetry::init_propagator()?;
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());

    let tracer = tracer_provider.tracer("");
    let otel_layer = init_tracing_opentelemetry::tracing_opentelemetry::layer()
        .with_error_records_to_exceptions(true)
        .with_tracer(tracer);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(otel_layer)
        .try_init()
        .context("init tracing subscriber")?;

    Ok((true, Some(TracerProviderGuard(tracer_provider))))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (otel_enabled, _tracer_guard) = init_logging_and_tracing().context("init tracing")?;

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
    server::run(cfg, spec, otel_enabled).await
}
