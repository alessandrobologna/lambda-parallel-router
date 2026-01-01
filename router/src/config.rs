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

#[derive(Debug, Clone, Deserialize)]
pub struct RouterConfig {
    pub listen_addr: SocketAddr,
    pub spec_path: PathBuf,

    #[serde(default)]
    pub aws_region: Option<String>,

    #[serde(default = "default_max_inflight_invocations")]
    pub max_inflight_invocations: usize,

    #[serde(default = "default_max_queue_depth_per_key")]
    pub max_queue_depth_per_key: usize,

    #[serde(default = "default_idle_ttl_ms")]
    pub idle_ttl_ms: u64,

    #[serde(default = "default_default_timeout_ms")]
    pub default_timeout_ms: u64,

    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
}

impl RouterConfig {
    pub fn from_yaml_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_yaml::from_slice(bytes)?)
    }
}
