#[tokio::main]
async fn main() -> anyhow::Result<()> {
    lpr_runtime_api_proxy::extension::run().await
}
