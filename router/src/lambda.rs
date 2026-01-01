use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::spec::InvokeMode;

pub enum LambdaInvokeResult {
    Buffered(Bytes),
    ResponseStream(mpsc::Receiver<anyhow::Result<Bytes>>),
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
                let out = self
                    .client
                    .invoke_with_response_stream()
                    .function_name(function_name)
                    .payload(aws_sdk_lambda::primitives::Blob::new(payload))
                    .send()
                    .await?;

                if out.status_code() != 200 {
                    anyhow::bail!(
                        "InvokeWithResponseStream failed for {function_name} (status {})",
                        out.status_code()
                    );
                }

                let stream = out.event_stream;
                let (tx, rx) = mpsc::channel::<anyhow::Result<Bytes>>(16);

                tokio::spawn(async move {
                    let mut stream = stream;
                    loop {
                        let event = match stream.recv().await {
                            Ok(Some(e)) => e,
                            Ok(None) => break,
                            Err(err) => {
                                let _ = tx
                                    .send(Err(anyhow::anyhow!("event stream recv: {err}")))
                                    .await;
                                break;
                            }
                        };

                        match event {
                            aws_sdk_lambda::types::InvokeWithResponseStreamResponseEvent::PayloadChunk(
                                chunk,
                            ) => {
                                let Some(payload) = chunk.payload() else {
                                    continue;
                                };
                                let bytes = Bytes::copy_from_slice(payload.as_ref());
                                if tx.send(Ok(bytes)).await.is_err() {
                                    break;
                                }
                            }
                            aws_sdk_lambda::types::InvokeWithResponseStreamResponseEvent::InvokeComplete(
                                complete,
                            ) => {
                                if let Some(code) = complete.error_code() {
                                    let details = complete.error_details().unwrap_or_default();
                                    let _ = tx
                                        .send(Err(anyhow::anyhow!(
                                            "lambda stream error ({code}): {details}"
                                        )))
                                        .await;
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                });

                Ok(LambdaInvokeResult::ResponseStream(rx))
            }
        }
    }
}
