//! Loading configuration/spec documents from either the local filesystem or S3.
//!
//! This enables deployments where the router container is generic and the routing config/spec are
//! provided dynamically (for example, uploaded to S3 at deploy-time and referenced via env vars).

use std::{path::PathBuf, str::FromStr};

use anyhow::Context;
use bytes::Bytes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentLocation {
    File(PathBuf),
    S3 { bucket: String, key: String },
}

impl DocumentLocation {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        if let Ok(url) = url::Url::parse(s) {
            if url.scheme() == "s3" {
                let bucket = url
                    .host_str()
                    .context("s3 uri must include a bucket name")?
                    .to_string();
                let key = url.path().trim_start_matches('/').to_string();
                if key.is_empty() {
                    anyhow::bail!("s3 uri must include a key (path)");
                }
                return Ok(Self::S3 { bucket, key });
            }
        }
        Ok(Self::File(PathBuf::from_str(s)?))
    }

    pub async fn read_bytes(&self, s3: Option<&aws_sdk_s3::Client>) -> anyhow::Result<Bytes> {
        match self {
            Self::File(path) => Ok(Bytes::from(tokio::fs::read(path).await?)),
            Self::S3 { bucket, key } => {
                let s3 = s3.context("S3 client not configured")?;
                let out = s3
                    .get_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .with_context(|| format!("get_object s3://{bucket}/{key}"))?;
                let bytes = out
                    .body
                    .collect()
                    .await
                    .context("collect get_object body")?
                    .into_bytes();
                Ok(bytes)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_s3_uri() {
        assert_eq!(
            DocumentLocation::parse("s3://my-bucket/a/b/c.yaml").unwrap(),
            DocumentLocation::S3 {
                bucket: "my-bucket".to_string(),
                key: "a/b/c.yaml".to_string()
            }
        );
    }

    #[test]
    fn parse_file_path() {
        assert_eq!(
            DocumentLocation::parse("examples/router.yaml").unwrap(),
            DocumentLocation::File(PathBuf::from("examples/router.yaml"))
        );
    }

    #[tokio::test]
    async fn file_read_works() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("lpr-test-{}.txt", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, b"hello").await.unwrap();

        let loc = DocumentLocation::File(path.clone());
        let bytes = loc.read_bytes(None).await.unwrap();
        assert_eq!(bytes.as_ref(), b"hello");

        let _ = tokio::fs::remove_file(path).await;
    }
}
