use anyhow::Context;
use clap::Parser;
use opentelemetry::trace::TracerProvider as _;
use tracing_subscriber::layer::SubscriberExt as _;

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

    // App Runner's managed OTEL collector is trace-only (AWS X-Ray). Disable metrics by default.
    if env_missing_or_empty("OTEL_METRICS_EXPORTER") {
        std::env::set_var("OTEL_METRICS_EXPORTER", "none");
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

fn resolve_host_port(endpoint: &str) -> Option<String> {
    if let Ok(url) = url::Url::parse(endpoint) {
        let host = url.host_str()?;
        let port = url.port_or_known_default()?;
        return Some(format!("{host}:{port}"));
    }

    // Fallback for endpoints configured without a scheme (e.g. `localhost:4317`).
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.contains(':') {
        return Some(trimmed.to_string());
    }

    None
}

async fn probe_otlp_collector() {
    let endpoint = match otlp_traces_endpoint_env() {
        Some(v) => v,
        None => return,
    };
    let addr = match resolve_host_port(&endpoint) {
        Some(v) => v,
        None => {
            tracing::warn!(
                event = "otel_probe_invalid_endpoint",
                otlp_endpoint = %endpoint,
                "failed to parse OTLP endpoint"
            );
            return;
        }
    };

    let timeout = std::time::Duration::from_secs(1);
    let connect = tokio::net::TcpStream::connect(&addr);
    match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(_)) => {
            tracing::debug!(
                event = "otel_probe",
                otlp_endpoint = %endpoint,
                otlp_addr = %addr,
                "connected to OTLP collector"
            );
        }
        Ok(Err(err)) => {
            tracing::warn!(
                event = "otel_probe_failed",
                otlp_endpoint = %endpoint,
                otlp_addr = %addr,
                error = ?err,
                "failed to connect to OTLP collector"
            );
        }
        Err(err) => {
            tracing::warn!(
                event = "otel_probe_failed",
                otlp_endpoint = %endpoint,
                otlp_addr = %addr,
                error = ?err,
                "failed to connect to OTLP collector"
            );
        }
    }
}

fn init_tracing() -> anyhow::Result<TracerProviderGuard> {
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

    let _log_guard = init_tracing_opentelemetry::TracingConfig::production()
        .with_file_names(false)
        .with_otel(false)
        .init_subscriber_ext(|subscriber| subscriber.with(otel_layer))
        .context("init tracing subscriber")?;

    Ok(TracerProviderGuard(tracer_provider))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    configure_otel_env_defaults();

    let _tracer_guard = init_tracing().context("init tracing")?;
    probe_otlp_collector().await;

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
