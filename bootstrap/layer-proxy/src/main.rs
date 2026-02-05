#[tokio::main]
async fn main() -> anyhow::Result<()> {
    smug_runtime_api_proxy::extension::run().await
}
