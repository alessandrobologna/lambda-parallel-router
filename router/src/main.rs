use anyhow::Context;
use clap::Parser;
use opentelemetry::{
    propagation::TextMapCompositePropagator,
    KeyValue,
};
use serde_json::Value as JsonValue;
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

fn otlp_traces_protocol_env() -> Option<String> {
    [
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
    ]
    .into_iter()
    .find_map(|name| std::env::var(name).ok())
    .map(|v| v.trim().to_ascii_lowercase())
    .filter(|v| !v.is_empty())
}

fn default_otlp_traces_protocol() -> String {
    match std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(endpoint) if endpoint.contains("localhost:4317") => "grpc".to_string(),
        _ => "http/protobuf".to_string(),
    }
}

fn url_encode_header_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        let is_unreserved = matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~');
        if is_unreserved {
            out.push(b as char);
        } else {
            use std::fmt::Write as _;
            write!(&mut out, "%{:02X}", b).expect("writing into a String can't fail");
        }
    }
    out
}

fn try_set_otlp_headers_from_json_secret() -> anyhow::Result<()> {
    if !env_missing_or_empty("OTEL_EXPORTER_OTLP_HEADERS")
        || !env_missing_or_empty("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
    {
        return Ok(());
    }

    let raw = match std::env::var("LPR_OTEL_HEADERS_JSON") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Ok(()),
    };

    let parsed: JsonValue =
        serde_json::from_str(&raw).context("parse LPR_OTEL_HEADERS_JSON as JSON")?;
    let obj = parsed
        .as_object()
        .context("LPR_OTEL_HEADERS_JSON must be a JSON object")?;

    let mut parts: Vec<String> = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let key = k.trim();
        if key.is_empty() {
            anyhow::bail!("LPR_OTEL_HEADERS_JSON contains an empty header name");
        }
        let value = v
            .as_str()
            .context("LPR_OTEL_HEADERS_JSON header values must be strings")?
            .trim();
        if value.is_empty() {
            anyhow::bail!("LPR_OTEL_HEADERS_JSON contains an empty value for header {key}");
        }
        parts.push(format!("{key}={}", url_encode_header_value(value)));
    }

    if parts.is_empty() {
        anyhow::bail!("LPR_OTEL_HEADERS_JSON must contain at least one header");
    }

    std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", parts.join(","));
    Ok(())
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

    // If OTEL is enabled, provide sane defaults for App Runner X-Ray integration. The exporter endpoint
    // must be configured explicitly via env vars (for example `OTEL_EXPORTER_OTLP_ENDPOINT`).
    if env_missing_or_empty("OTEL_PROPAGATORS") {
        std::env::set_var("OTEL_PROPAGATORS", "xray,tracecontext,baggage");
    }
    if env_missing_or_empty("OTEL_METRICS_EXPORTER") {
        std::env::set_var("OTEL_METRICS_EXPORTER", "none");
    }

    try_set_otlp_headers_from_json_secret().context("configure OTLP headers")?;

    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_NAME").to_string());
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name)
        .with_attributes([
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("cloud.provider", "aws"),
            KeyValue::new("cloud.platform", "aws_app_runner"),
        ])
        .build();

    let protocol = otlp_traces_protocol_env().unwrap_or_else(default_otlp_traces_protocol);
    let exporter = match protocol.as_str() {
        "grpc" => opentelemetry_otlp::SpanExporter::builder().with_tonic().build(),
        "http/protobuf" => opentelemetry_otlp::SpanExporter::builder().with_http().build(),
        other => anyhow::bail!("unsupported OTLP traces protocol: {other}"),
    }
    .context("build OTLP span exporter")?;

    let batch =
        opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor::builder(
            exporter,
            opentelemetry_sdk::runtime::Tokio,
        )
        .build();

    let mut tracer_provider_builder = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(batch);

    if std::env::var("LPR_OBSERVABILITY_VENDOR")
        .ok()
        .map(|v| v.trim().to_string())
        .as_deref()
        == Some("AWSXRAY")
    {
        tracer_provider_builder = tracer_provider_builder
            .with_id_generator(opentelemetry_aws::trace::XrayIdGenerator::default());
    }

    let tracer_provider = tracer_provider_builder.build();

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    opentelemetry::global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
        Box::new(opentelemetry_aws::trace::XrayPropagator::default()),
        Box::new(opentelemetry_sdk::propagation::TraceContextPropagator::new()),
        Box::new(opentelemetry_sdk::propagation::BaggagePropagator::new()),
    ]));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
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
