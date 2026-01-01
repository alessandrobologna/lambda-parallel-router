//! Router configuration loaded from YAML.
//!
//! This config is intentionally small and focused on batching behavior and safe defaults.

use std::{net::SocketAddr, path::PathBuf};

use serde::Deserialize;

fn default_max_inflight_invocations() -> usize {
    64
}

fn default_max_queue_depth_per_key() -> usize {
    1000
}

fn default_idle_ttl_ms() -> u64 {
    30_000
}

fn default_default_timeout_ms() -> u64 {
    2_000
}

fn default_max_body_bytes() -> usize {
    1024 * 1024
}

fn default_max_invoke_payload_bytes() -> usize {
    6 * 1024 * 1024
}

#[derive(Debug, Clone, Deserialize)]
/// Top-level router configuration.
pub struct RouterConfig {
    /// Address the router listens on (e.g. `127.0.0.1:3000`).
    pub listen_addr: SocketAddr,
    /// Path to the OpenAPI-ish spec YAML file.
    pub spec_path: PathBuf,

    #[serde(default)]
    /// Optional AWS region override for the Lambda client.
    pub aws_region: Option<String>,

    #[serde(default = "default_max_inflight_invocations")]
    /// Maximum number of concurrent in-flight Lambda invocations across all routes.
    pub max_inflight_invocations: usize,

    #[serde(default = "default_max_queue_depth_per_key")]
    /// Per-batch-key enqueue depth. When full, requests are rejected with 503.
    pub max_queue_depth_per_key: usize,

    #[serde(default = "default_idle_ttl_ms")]
    /// If a batch key sees no traffic for this long, its batching task is evicted.
    pub idle_ttl_ms: u64,

    #[serde(default = "default_default_timeout_ms")]
    /// Default per-request timeout (used when an operation doesn't specify `x-lpr.timeout_ms`).
    pub default_timeout_ms: u64,

    #[serde(default = "default_max_body_bytes")]
    /// Maximum accepted request body size.
    pub max_body_bytes: usize,

    #[serde(default = "default_max_invoke_payload_bytes")]
    /// Maximum JSON payload size sent to Lambda per invocation.
    ///
    /// If a batch exceeds this limit, the router will split it into multiple invocations when
    /// possible; otherwise the affected requests will fail.
    pub max_invoke_payload_bytes: usize,
}

impl RouterConfig {
    /// Parse a YAML router config from bytes.
    pub fn from_yaml_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_yaml::from_slice(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_for_optional_fields() {
        let yaml = br#"
listen_addr: "127.0.0.1:3000"
spec_path: "spec.yaml"
"#;
        let cfg = RouterConfig::from_yaml_bytes(yaml).unwrap();
        assert_eq!(cfg.max_inflight_invocations, 64);
        assert_eq!(cfg.max_queue_depth_per_key, 1000);
        assert_eq!(cfg.idle_ttl_ms, 30_000);
        assert_eq!(cfg.default_timeout_ms, 2_000);
        assert_eq!(cfg.max_body_bytes, 1024 * 1024);
        assert_eq!(cfg.max_invoke_payload_bytes, 6 * 1024 * 1024);
        assert!(cfg.aws_region.is_none());
    }
}
