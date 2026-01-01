use async_trait::async_trait;
use bytes::Bytes;

use crate::spec::InvokeMode;

pub enum LambdaInvokeResult {
    Buffered(Bytes),
}

#[async_trait]
pub trait LambdaInvoker: Send + Sync {
    async fn invoke(
        &self,
        function_name: &str,
        payload: Bytes,
        mode: InvokeMode,
    ) -> anyhow::Result<LambdaInvokeResult>;
}

pub struct AwsLambdaInvoker {
    client: aws_sdk_lambda::Client,
}

impl AwsLambdaInvoker {
    pub async fn new(region: Option<String>) -> anyhow::Result<Self> {
        let mut loader = aws_config::from_env();
        if let Some(region) = region {
            loader = loader.region(aws_config::Region::new(region));
        }
        let cfg = loader.load().await;
        let client = aws_sdk_lambda::Client::new(&cfg);
        Ok(Self { client })
    }
}

#[async_trait]
impl LambdaInvoker for AwsLambdaInvoker {
    async fn invoke(
        &self,
        function_name: &str,
        payload: Bytes,
        mode: InvokeMode,
    ) -> anyhow::Result<LambdaInvokeResult> {
        match mode {
            InvokeMode::Buffered => {
                let out = self
                    .client
                    .invoke()
                    .function_name(function_name)
                    .payload(aws_sdk_lambda::primitives::Blob::new(payload))
                    .send()
                    .await?;

                if let Some(function_error) = out.function_error() {
                    anyhow::bail!("lambda function error ({function_error}) for {function_name}");
                }

                let bytes = out
                    .payload()
                    .map(|b| Bytes::copy_from_slice(b.as_ref()))
                    .unwrap_or_default();

                Ok(LambdaInvokeResult::Buffered(bytes))
            }
            InvokeMode::ResponseStream => {
                anyhow::bail!("InvokeMode::ResponseStream not implemented yet")
            }
        }
    }
}
