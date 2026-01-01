//! OpenAPI-ish routing spec parsing and matching.
//!
//! The router uses a lightweight subset of OpenAPI (`paths` + HTTP methods) and relies on vendor
//! extensions to define Lambda targets and microbatching behavior.

use std::collections::{BTreeMap, HashMap};

use http::Method;
use matchit::Router;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
/// How the router invokes Lambda for an operation.
#[serde(rename_all = "snake_case")]
pub enum InvokeMode {
    /// Synchronous `Invoke` returning a single buffered payload.
    Buffered,
    /// `InvokeWithResponseStream` returning a byte stream (used with NDJSON records).
    ResponseStream,
}

impl Default for InvokeMode {
    fn default() -> Self {
        Self::Buffered
    }
}

#[derive(Debug, Clone, Deserialize)]
/// `x-lpr` vendor extension (per operation).
pub struct LprOperationConfig {
    /// Maximum time (in milliseconds) to wait before flushing a batch.
    pub max_wait_ms: u64,
    /// Maximum number of requests per batch.
    pub max_batch_size: usize,

    #[serde(default)]
    /// Optional per-request timeout override (milliseconds).
    pub timeout_ms: Option<u64>,

    #[serde(default)]
    /// Optional invocation mode override (defaults to buffered).
    pub invoke_mode: InvokeMode,
}

#[derive(Debug, Clone, Deserialize)]
/// One operation entry under a path item.
pub struct Operation {
    #[serde(rename = "operationId", default)]
    pub operation_id: Option<String>,

    #[serde(rename = "x-target-lambda")]
    /// Lambda function name or ARN.
    pub target_lambda: String,

    #[serde(rename = "x-lpr")]
    /// Router-specific operation configuration.
    pub lpr: LprOperationConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
/// OpenAPI-ish "path item" containing method operations.
pub struct PathItem {
    #[serde(default)]
    pub get: Option<Operation>,
    #[serde(default)]
    pub post: Option<Operation>,
    #[serde(default)]
    pub put: Option<Operation>,
    #[serde(default)]
    pub delete: Option<Operation>,
    #[serde(default)]
    pub patch: Option<Operation>,
    #[serde(default)]
    pub head: Option<Operation>,
    #[serde(default)]
    pub options: Option<Operation>,
}

#[derive(Debug, Clone, Deserialize)]
/// Minimal OpenAPI-ish spec containing only `paths`.
pub struct OpenApiLikeSpec {
    pub paths: BTreeMap<String, PathItem>,
}

#[derive(Debug, Clone)]
/// Fully resolved per-operation configuration used by the router at runtime.
pub struct OperationConfig {
    pub route_template: String,
    pub method: Method,
    pub operation_id: Option<String>,
    pub target_lambda: String,
    pub max_wait_ms: u64,
    pub max_batch_size: usize,
    pub timeout_ms: u64,
    pub invoke_mode: InvokeMode,
}

#[derive(Debug, Clone)]
struct RouteConfig {
    ops_by_method: HashMap<Method, OperationConfig>,
}

#[derive(Debug, Clone)]
/// A compiled route matcher.
pub struct CompiledSpec {
    router: Router<RouteConfig>,
}

#[derive(Debug, Clone)]
/// Outcome of matching an incoming request to a configured operation.
pub enum RouteMatch<'a> {
    /// No configured path matched.
    NotFound,
    /// Path matched but method wasn't configured.
    MethodNotAllowed { allowed: Vec<Method> },
    /// Path+method matched and produced an operation config.
    Matched(&'a OperationConfig),
}

impl CompiledSpec {
    /// Parse and compile a YAML spec into a matcher.
    pub fn from_yaml_bytes(bytes: &[u8], default_timeout_ms: u64) -> anyhow::Result<Self> {
        let spec: OpenApiLikeSpec = serde_yaml::from_slice(bytes)?;
        Self::compile(spec, default_timeout_ms)
    }

    /// Match an `(HTTP method, path)` pair.
    ///
    /// The router uses this to decide which Lambda to invoke, and which batching settings to apply.
    pub fn match_request<'a>(&'a self, method: &Method, path: &str) -> RouteMatch<'a> {
        let Ok(matched) = self.router.at(path) else {
            return RouteMatch::NotFound;
        };

        match matched.value.ops_by_method.get(method) {
            Some(op) => RouteMatch::Matched(op),
            None => RouteMatch::MethodNotAllowed {
                allowed: {
                    let mut methods: Vec<Method> =
                        matched.value.ops_by_method.keys().cloned().collect();
                    methods.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                    methods
                },
            },
        }
    }

    fn compile(spec: OpenApiLikeSpec, default_timeout_ms: u64) -> anyhow::Result<Self> {
        let mut router = Router::new();
        for (route_template, item) in spec.paths {
            let matchit_path = openapi_path_to_matchit(&route_template)?;
            let mut ops_by_method = HashMap::new();
            add_op(
                &mut ops_by_method,
                &route_template,
                Method::GET,
                item.get,
                default_timeout_ms,
            )?;
            add_op(
                &mut ops_by_method,
                &route_template,
                Method::POST,
                item.post,
                default_timeout_ms,
            )?;
            add_op(
                &mut ops_by_method,
                &route_template,
                Method::PUT,
                item.put,
                default_timeout_ms,
            )?;
            add_op(
                &mut ops_by_method,
                &route_template,
                Method::DELETE,
                item.delete,
                default_timeout_ms,
            )?;
            add_op(
                &mut ops_by_method,
                &route_template,
                Method::PATCH,
                item.patch,
                default_timeout_ms,
            )?;
            add_op(
                &mut ops_by_method,
                &route_template,
                Method::HEAD,
                item.head,
                default_timeout_ms,
            )?;
            add_op(
                &mut ops_by_method,
                &route_template,
                Method::OPTIONS,
                item.options,
                default_timeout_ms,
            )?;

            if ops_by_method.is_empty() {
                continue;
            }

            router.insert(matchit_path, RouteConfig { ops_by_method })?;
        }
        Ok(Self { router })
    }
}

fn add_op(
    map: &mut HashMap<Method, OperationConfig>,
    route_template: &str,
    method: Method,
    op: Option<Operation>,
    default_timeout_ms: u64,
) -> anyhow::Result<()> {
    let Some(op) = op else {
        return Ok(());
    };

    if op.lpr.max_batch_size == 0 {
        anyhow::bail!("x-lpr.max_batch_size must be > 0 for {method} {route_template}");
    }

    let timeout_ms = op.lpr.timeout_ms.unwrap_or(default_timeout_ms);
    map.insert(
        method.clone(),
        OperationConfig {
            route_template: route_template.to_string(),
            method,
            operation_id: op.operation_id,
            target_lambda: op.target_lambda,
            max_wait_ms: op.lpr.max_wait_ms,
            max_batch_size: op.lpr.max_batch_size,
            timeout_ms,
            invoke_mode: op.lpr.invoke_mode,
        },
    );
    Ok(())
}

fn openapi_path_to_matchit(path: &str) -> anyhow::Result<String> {
    if !path.starts_with('/') {
        anyhow::bail!("path templates must start with '/': {path}");
    }

    let mut in_param = false;
    let mut param_name = String::new();
    for ch in path.chars() {
        match ch {
            '{' => {
                if in_param {
                    anyhow::bail!("nested '{{' in path template: {path}");
                }
                in_param = true;
                param_name.clear();
            }
            '}' => {
                if !in_param {
                    anyhow::bail!("unmatched '}}' in path template: {path}");
                }
                if param_name.is_empty() {
                    anyhow::bail!("empty '{{}}' param in path template: {path}");
                }
                in_param = false;
            }
            _ => {
                if in_param {
                    param_name.push(ch);
                }
            }
        }
    }

    if in_param {
        anyhow::bail!("unclosed '{{' in path template: {path}");
    }

    // OpenAPI-style templates already match `matchit`'s `{param}` syntax.
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_openapi_paths_to_matchit() {
        assert_eq!(
            openapi_path_to_matchit("/v1/items/{id}").unwrap(),
            "/v1/items/{id}"
        );
    }

    #[test]
    fn matches_compiled_route_by_method() {
        let yaml = br#"
paths:
  /v1/items/{id}:
    get:
      operationId: getItem
      x-target-lambda: my-fn
      x-lpr:
        max_wait_ms: 10
        max_batch_size: 16
        timeout_ms: 2000
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 2500).unwrap();
        let RouteMatch::Matched(op) = spec.match_request(&Method::GET, "/v1/items/123") else {
            panic!("expected route match");
        };

        assert_eq!(op.route_template, "/v1/items/{id}");
        assert_eq!(op.target_lambda, "my-fn");
        assert_eq!(op.max_wait_ms, 10);
        assert_eq!(op.max_batch_size, 16);
        assert_eq!(op.timeout_ms, 2000);
        assert_eq!(op.operation_id.as_deref(), Some("getItem"));

        assert!(matches!(
            spec.match_request(&Method::POST, "/v1/items/123"),
            RouteMatch::MethodNotAllowed { .. }
        ));
    }

    #[test]
    fn method_not_allowed_returns_sorted_allow_list() {
        let yaml = br#"
paths:
  /x:
    post:
      x-target-lambda: fn
      x-lpr: { max_wait_ms: 1, max_batch_size: 1 }
    get:
      x-target-lambda: fn
      x-lpr: { max_wait_ms: 1, max_batch_size: 1 }
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 1000).unwrap();
        let RouteMatch::MethodNotAllowed { allowed } = spec.match_request(&Method::PUT, "/x")
        else {
            panic!("expected method not allowed");
        };
        assert_eq!(allowed, vec![Method::GET, Method::POST]);
    }

    #[test]
    fn rejects_invalid_path_templates() {
        assert!(openapi_path_to_matchit("v1/items").is_err());
        assert!(openapi_path_to_matchit("/v1/{").is_err());
        assert!(openapi_path_to_matchit("/v1/}").is_err());
        assert!(openapi_path_to_matchit("/v1/{}").is_err());
        assert!(openapi_path_to_matchit("/v1/{{id}}").is_err());
    }

    #[test]
    fn ignores_paths_with_no_operations() {
        let yaml = br#"
paths:
  /empty: {}
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 1000).unwrap();
        assert!(matches!(
            spec.match_request(&Method::GET, "/empty"),
            RouteMatch::NotFound
        ));
    }

    #[test]
    fn max_batch_size_must_be_positive() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: fn
      x-lpr: { max_wait_ms: 1, max_batch_size: 0 }
"#;
        assert!(CompiledSpec::from_yaml_bytes(yaml, 1000).is_err());
    }

    #[test]
    fn invoke_mode_defaults_to_buffered() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: fn
      x-lpr:
        max_wait_ms: 1
        max_batch_size: 1
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 1000).unwrap();
        let RouteMatch::Matched(op) = spec.match_request(&Method::GET, "/x") else {
            panic!("expected match");
        };
        assert_eq!(op.invoke_mode, InvokeMode::Buffered);
    }
}
