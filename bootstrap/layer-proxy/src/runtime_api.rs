use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt as _;

#[derive(Clone)]
pub struct RuntimeApiClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Debug)]
pub struct NextInvocation {
    pub request_id: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl RuntimeApiClient {
    pub fn new(base_url: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().http1_only().build()?;
        Ok(Self { base_url, http })
    }

    pub async fn next_invocation(&self) -> anyhow::Result<NextInvocation> {
        let url = format!("{}/2018-06-01/runtime/invocation/next", self.base_url);
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("upstream /next failed (status {status})");
        }

        let headers = resp.headers().clone();
        let request_id = headers
            .get("Lambda-Runtime-Aws-Request-Id")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("upstream /next missing Lambda-Runtime-Aws-Request-Id"))?
            .to_string();

        let body = resp.bytes().await?;

        Ok(NextInvocation {
            request_id,
            headers,
            body,
        })
    }

    pub async fn forward_invocation_call(
        &self,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> anyhow::Result<StatusCode> {
        let url = format!("{}{}", self.base_url, path_and_query);
        let mut req = self.http.request(method, url);

        for (k, v) in headers.iter() {
            let name = k.as_str();
            if name.eq_ignore_ascii_case("host")
                || name.eq_ignore_ascii_case("connection")
                || name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("transfer-encoding")
            {
                continue;
            }

            req = req.header(k, v);
        }

        let resp = req.body(body).send().await?;
        Ok(StatusCode::from_u16(resp.status().as_u16())?)
    }

    pub async fn forward_request(
        &self,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> anyhow::Result<(StatusCode, HeaderMap, Bytes)> {
        let url = format!("{}{}", self.base_url, path_and_query);
        let mut req = self.http.request(method, url);

        for (k, v) in headers.iter() {
            let name = k.as_str();
            if name.eq_ignore_ascii_case("host")
                || name.eq_ignore_ascii_case("connection")
                || name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("transfer-encoding")
            {
                continue;
            }

            req = req.header(k, v);
        }

        let resp = req.body(body).send().await?;
        let status = StatusCode::from_u16(resp.status().as_u16())?;
        let headers = resp.headers().clone();
        let body = resp.bytes().await?;
        Ok((status, headers, body))
    }

    pub fn start_streaming_response(
        &self,
        request_id: String,
        content_type: &'static str,
    ) -> (
        mpsc::UnboundedSender<Bytes>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let url = format!(
            "{}/2018-06-01/runtime/invocation/{}/response",
            self.base_url, request_id
        );

        let client = self.http.clone();
        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();

        let join = tokio::spawn(async move {
            let body_stream =
                UnboundedReceiverStream::new(rx).map(Ok::<Bytes, std::convert::Infallible>);
            let resp = client
                .post(url)
                .header("Lambda-Runtime-Function-Response-Mode", "streaming")
                .header("Transfer-Encoding", "chunked")
                .header("Content-Type", content_type)
                .body(reqwest::Body::wrap_stream(body_stream))
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                anyhow::bail!("upstream /response failed (status {status})");
            }

            Ok(())
        });

        (tx, join)
    }

    pub async fn register_extension(&self, name: &str) -> anyhow::Result<String> {
        let url = format!("{}/2020-01-01/extension/register", self.base_url);
        let resp = self
            .http
            .post(url)
            .header("Lambda-Extension-Name", name)
            .json(&serde_json::json!({ "events": ["SHUTDOWN"] }))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("extension register failed (status {status})");
        }

        let id = resp
            .headers()
            .get("Lambda-Extension-Identifier")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("missing Lambda-Extension-Identifier header"))?
            .to_string();

        Ok(id)
    }

    pub async fn next_extension_event(&self, extension_id: &str) -> anyhow::Result<Bytes> {
        let url = format!("{}/2020-01-01/extension/event/next", self.base_url);
        let resp = self
            .http
            .get(url)
            .header("Lambda-Extension-Identifier", extension_id)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("extension event/next failed (status {status})");
        }

        Ok(resp.bytes().await?)
    }
}
