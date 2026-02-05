//! Lambda invocation abstraction.
//!
//! The gateway supports two modes:
//! - Buffered (`Invoke`) where the entire Lambda response is returned as a single payload.
//! - Response streaming (`InvokeWithResponseStream`) where the Lambda response is delivered as a
//!   stream of byte chunks (used for NDJSON records).

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

use crate::spec::InvokeMode;

/// Result of invoking a Lambda function.
pub enum LambdaInvokeResult {
    /// Entire response payload (buffered).
    Buffered(Bytes),
    /// Stream of payload chunks.
    ///
    /// The receiver yields `Ok(Bytes)` chunks, or a terminal `Err` if the stream reports an error.
    ResponseStream(mpsc::Receiver<anyhow::Result<Bytes>>),
}

#[async_trait]
/// Abstract Lambda invoker to allow unit-testing the gateway without AWS.
pub trait LambdaInvoker: Send + Sync {
    async fn invoke(
        &self,
        function_name: &str,
        payload: Bytes,
        mode: InvokeMode,
    ) -> anyhow::Result<LambdaInvokeResult>;
}

/// AWS SDK implementation of [`LambdaInvoker`].
pub struct AwsLambdaInvoker {
    client: aws_sdk_lambda::Client,
}

impl AwsLambdaInvoker {
    /// Create an invoker using standard AWS credential resolution.
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
                    let mut can_send = true;
                    loop {
                        let event = match stream.recv().await {
                            Ok(Some(e)) => e,
                            Ok(None) => break,
                            Err(err) => {
                                if can_send {
                                    let _ = tx
                                        .send(Err(anyhow::anyhow!("event stream recv: {err}")))
                                        .await;
                                }
                                break;
                            }
                        };

                        match event {
                            aws_sdk_lambda::types::InvokeWithResponseStreamResponseEvent::PayloadChunk(
                                chunk,
                            ) => {
                                if !can_send {
                                    continue;
                                }
                                let Some(payload) = chunk.payload() else {
                                    continue;
                                };
                                let bytes = Bytes::copy_from_slice(payload.as_ref());
                                if tx.send(Ok(bytes)).await.is_err() {
                                    // Downstream no longer needs the response body. Keep draining the
                                    // upstream event stream to avoid locally resetting the HTTP/2 stream.
                                    can_send = false;
                                }
                            }
                            aws_sdk_lambda::types::InvokeWithResponseStreamResponseEvent::InvokeComplete(
                                complete,
                            ) => {
                                if let Some(code) = complete.error_code() {
                                    let details = complete.error_details().unwrap_or_default();
                                    if can_send {
                                        let _ = tx
                                            .send(Err(anyhow::anyhow!(
                                                "lambda stream error ({code}): {details}"
                                            )))
                                            .await;
                                    }
                                }

                                // Best-effort: drain to EOF so the underlying HTTP stream can close
                                // cleanly. Dropping the event stream without draining results in a
                                // local reset (RST_STREAM), which can accumulate and trigger
                                // `h2::proto::streams::streams` warnings like:
                                // "locally-reset streams reached limit (1024)".
                                let _ = timeout(Duration::from_secs(1), async {
                                    while let Ok(Some(_)) = stream.recv().await {}
                                })
                                .await;
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
