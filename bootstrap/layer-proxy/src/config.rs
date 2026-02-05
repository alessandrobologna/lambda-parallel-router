use std::{net::SocketAddr, str::FromStr};

#[derive(Debug, Clone)]
pub struct Config {
    pub proxy_addr: SocketAddr,
    pub upstream_runtime_api: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let proxy_addr =
            std::env::var("SMUG_PROXY_ADDR").unwrap_or_else(|_| "127.0.0.1:9009".into());
        let proxy_addr = SocketAddr::from_str(&proxy_addr)
            .map_err(|err| anyhow::anyhow!("invalid SMUG_PROXY_ADDR ({proxy_addr}): {err}"))?;

        let upstream_runtime_api = std::env::var("SMUG_UPSTREAM_RUNTIME_API")
            .ok()
            .or_else(|| std::env::var("AWS_LAMBDA_RUNTIME_API").ok())
            .ok_or_else(|| {
                anyhow::anyhow!("missing SMUG_UPSTREAM_RUNTIME_API and AWS_LAMBDA_RUNTIME_API")
            })?;

        Ok(Self {
            proxy_addr,
            upstream_runtime_api,
        })
    }

    pub fn upstream_base_url(&self) -> String {
        format!("http://{}", self.upstream_runtime_api)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_proxy_addr() {
        std::env::remove_var("SMUG_PROXY_ADDR");
        std::env::set_var("SMUG_UPSTREAM_RUNTIME_API", "127.0.0.1:9001");

        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.proxy_addr, "127.0.0.1:9009".parse().unwrap());
        assert_eq!(cfg.upstream_runtime_api, "127.0.0.1:9001");
    }
}
