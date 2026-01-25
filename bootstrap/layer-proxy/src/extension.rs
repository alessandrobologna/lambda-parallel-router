use crate::config::Config;
use crate::proxy;
use crate::runtime_api::RuntimeApiClient;

const EXTENSION_NAME: &str = "lpr-runtime-api-proxy";

pub async fn run() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let upstream = RuntimeApiClient::new(cfg.upstream_base_url())?;

    init_tracing();

    let extension_id = upstream.register_extension(EXTENSION_NAME).await?;
    tracing::info!(extension_id = %extension_id, "registered extension");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let upstream_for_server = upstream.clone();
    let server = tokio::spawn(async move {
        proxy::serve(cfg.proxy_addr, upstream_for_server, shutdown_rx).await
    });

    let _event = upstream.next_extension_event(&extension_id).await?;
    tracing::info!("shutdown event received");

    let _ = shutdown_tx.send(());
    match server.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(anyhow::anyhow!("proxy server task failed: {err}")),
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let log_format = std::env::var("AWS_LAMBDA_LOG_FORMAT").unwrap_or_default();
    if log_format.eq_ignore_ascii_case("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
