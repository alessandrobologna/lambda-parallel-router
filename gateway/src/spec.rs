//! OpenAPI-ish routing spec parsing and matching.
//!
//! The gateway uses a lightweight subset of OpenAPI (`paths` + HTTP methods) and relies on vendor
//! extensions to define Lambda targets and microbatching behavior.

use std::collections::{BTreeMap, HashMap};

use http::{HeaderName, Method};
use matchit::Router;
use serde::Deserialize;

fn is_lambda_function_arn(value: &str) -> bool {
    // Expected shape:
    // arn:partition:lambda:region:account-id:function:function-name[:qualifier]
    let value = value.trim();
    if !value.starts_with("arn:") {
        return false;
    }
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() < 7 {
        return false;
    }
    if parts[2] != "lambda" {
        return false;
    }
    if parts[5] != "function" {
        return false;
    }
    !parts[6].is_empty()
}

fn default_dynamic_target_rps() -> f64 {
    50.0
}

fn default_dynamic_steepness() -> f64 {
    0.01
}

fn default_dynamic_sampling_interval_ms() -> u64 {
    100
}

fn default_dynamic_smoothing_samples() -> usize {
    10
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
/// How the gateway invokes Lambda for an operation.
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
/// Dynamic batching window configuration.
///
/// When set, the gateway computes a per-batch flush window in `[min_wait_ms, max_wait_ms]` based on
/// the request rate for the current batch key.
#[serde(rename_all = "camelCase")]
pub struct DynamicWaitConfig {
    /// Minimum time (in milliseconds) to wait before flushing a batch.
    #[serde(deserialize_with = "crate::serde_ext::de_u64_or_string")]
    pub min_wait_ms: u64,

    /// Request rate (requests/sec) where the sigmoid is centered.
    #[serde(
        default = "default_dynamic_target_rps",
        deserialize_with = "crate::serde_ext::de_f64_or_string"
    )]
    pub target_rps: f64,

    /// Sigmoid steepness around `target_rps`.
    #[serde(
        default = "default_dynamic_steepness",
        deserialize_with = "crate::serde_ext::de_f64_or_string"
    )]
    pub steepness: f64,

    /// Sampling period for request counts (milliseconds).
    #[serde(
        default = "default_dynamic_sampling_interval_ms",
        deserialize_with = "crate::serde_ext::de_u64_or_string"
    )]
    pub sampling_interval_ms: u64,

    /// Moving average window size (number of samples).
    #[serde(
        default = "default_dynamic_smoothing_samples",
        deserialize_with = "crate::serde_ext::de_usize_or_string"
    )]
    pub smoothing_samples: usize,
}

#[derive(Debug, Clone, Deserialize)]
/// `x-smug` vendor extension (per operation).
#[serde(rename_all = "camelCase")]
pub struct SmugOperationConfig {
    /// Maximum time (in milliseconds) to wait before flushing a batch.
    #[serde(deserialize_with = "crate::serde_ext::de_u64_or_string")]
    pub max_wait_ms: u64,
    /// Maximum number of requests per batch.
    #[serde(deserialize_with = "crate::serde_ext::de_usize_or_string")]
    pub max_batch_size: usize,

    #[serde(default)]
    /// Optional additional batch key dimensions (e.g. `header:x-tenant-id`).
    ///
    /// The gateway always partitions batches by `(target_lambda, method, route_template)`.
    /// This list allows adding extra per-request dimensions to avoid mixing requests whose
    /// semantics differ (e.g. multi-tenant traffic).
    pub key: Vec<String>,

    #[serde(
        default,
        deserialize_with = "crate::serde_ext::de_option_u64_or_string"
    )]
    /// Optional per-request timeout override (milliseconds).
    pub timeout_ms: Option<u64>,

    #[serde(default)]
    /// Optional invocation mode override (defaults to buffered).
    pub invoke_mode: InvokeMode,

    #[serde(default)]
    /// Optional dynamic batching configuration (sigmoid-based).
    pub dynamic_wait: Option<DynamicWaitConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// One additional component of a batch key.
pub enum BatchKeyDimension {
    /// Partition batches by the value of a request header.
    Header(HeaderName),
    /// Partition batches by the value of a single query parameter.
    Query(String),
}

#[derive(Debug, Clone, Deserialize)]
/// One operation entry under a path item.
pub struct Operation {
    #[serde(rename = "operationId", default)]
    pub operation_id: Option<String>,

    #[serde(rename = "x-target-lambda")]
    /// Lambda function ARN.
    pub target_lambda: String,

    #[serde(rename = "x-smug")]
    /// Gateway-specific operation configuration.
    pub smug: SmugOperationConfig,
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
/// Fully resolved per-operation configuration used by the gateway at runtime.
pub struct OperationConfig {
    pub route_template: String,
    pub method: Method,
    pub operation_id: Option<String>,
    pub target_lambda: String,
    pub max_wait_ms: u64,
    pub max_batch_size: usize,
    pub key: Vec<BatchKeyDimension>,
    pub timeout_ms: u64,
    pub invoke_mode: InvokeMode,
    pub dynamic_wait: Option<DynamicWaitConfig>,
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
    Matched {
        op: &'a OperationConfig,
        path_params: HashMap<String, String>,
    },
}

impl CompiledSpec {
    /// Parse and compile a YAML spec into a matcher.
    pub fn from_yaml_bytes(bytes: &[u8], default_timeout_ms: u64) -> anyhow::Result<Self> {
        let spec: OpenApiLikeSpec = serde_yaml::from_slice(bytes)?;
        Self::compile(spec, default_timeout_ms)
    }

    /// Compile a parsed OpenAPI-ish spec into a matcher.
    pub fn from_spec(spec: OpenApiLikeSpec, default_timeout_ms: u64) -> anyhow::Result<Self> {
        Self::compile(spec, default_timeout_ms)
    }

    /// Match an `(HTTP method, path)` pair.
    ///
    /// The gateway uses this to decide which Lambda to invoke, and which batching settings to apply.
    pub fn match_request<'a>(&'a self, method: &Method, path: &str) -> RouteMatch<'a> {
        let Ok(matched) = self.router.at(path) else {
            return RouteMatch::NotFound;
        };

        match matched.value.ops_by_method.get(method) {
            Some(op) => {
                let mut path_params = HashMap::new();
                for (k, v) in matched.params.iter() {
                    path_params.insert(k.to_string(), v.to_string());
                }
                RouteMatch::Matched { op, path_params }
            }
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

    if !is_lambda_function_arn(&op.target_lambda) {
        anyhow::bail!(
            "x-target-lambda must be a Lambda function ARN for {method} {route_template} (got: {})",
            op.target_lambda
        );
    }

    if op.smug.max_batch_size == 0 {
        anyhow::bail!("x-smug.maxBatchSize must be > 0 for {method} {route_template}");
    }

    let key = parse_batch_key_dimensions(&op.smug.key)
        .map_err(|err| anyhow::anyhow!("invalid x-smug.key for {method} {route_template}: {err}"))?;

    let timeout_ms = op.smug.timeout_ms.unwrap_or(default_timeout_ms);

    if let Some(dynamic) = &op.smug.dynamic_wait {
        if dynamic.min_wait_ms > op.smug.max_wait_ms {
            anyhow::bail!(
                "x-smug.dynamicWait.minWaitMs must be <= x-smug.maxWaitMs for {method} {route_template}"
            );
        }

        if dynamic.sampling_interval_ms == 0 {
            anyhow::bail!(
                "x-smug.dynamicWait.samplingIntervalMs must be > 0 for {method} {route_template}"
            );
        }

        if dynamic.smoothing_samples == 0 {
            anyhow::bail!(
                "x-smug.dynamicWait.smoothingSamples must be > 0 for {method} {route_template}"
            );
        }

        if !dynamic.target_rps.is_finite() || dynamic.target_rps < 0.0 {
            anyhow::bail!(
                "x-smug.dynamicWait.targetRps must be a finite non-negative number for {method} {route_template}"
            );
        }

        if !dynamic.steepness.is_finite() || dynamic.steepness <= 0.0 {
            anyhow::bail!(
                "x-smug.dynamicWait.steepness must be a finite number > 0 for {method} {route_template}"
            );
        }
    }

    map.insert(
        method.clone(),
        OperationConfig {
            route_template: route_template.to_string(),
            method,
            operation_id: op.operation_id,
            target_lambda: op.target_lambda,
            max_wait_ms: op.smug.max_wait_ms,
            max_batch_size: op.smug.max_batch_size,
            key,
            timeout_ms,
            invoke_mode: op.smug.invoke_mode,
            dynamic_wait: op.smug.dynamic_wait,
        },
    );
    Ok(())
}

fn parse_batch_key_dimensions(raw: &[String]) -> anyhow::Result<Vec<BatchKeyDimension>> {
    let mut dims = Vec::with_capacity(raw.len());
    let mut headers = std::collections::HashSet::new();
    let mut queries = std::collections::HashSet::new();

    for entry in raw {
        let entry = entry.trim();
        if entry.is_empty() {
            anyhow::bail!("empty key entry");
        }

        // The spec draft examples sometimes include `method`/`route` explicitly, but the gateway
        // always keys by these fields anyway.
        let lowered = entry.to_ascii_lowercase();
        if lowered == "method"
            || lowered == "route"
            || lowered == "lambda"
            || lowered == "target_lambda"
            || lowered == "target-lambda"
        {
            continue;
        }

        let Some((kind, rest)) = entry.split_once(':') else {
            anyhow::bail!("unsupported key dimension: {entry}");
        };

        match kind.trim().to_ascii_lowercase().as_str() {
            "header" => {
                let name = rest.trim();
                if name.is_empty() {
                    anyhow::bail!("header dimension requires a name");
                }
                let normalized = name.to_ascii_lowercase();
                let header = HeaderName::from_bytes(normalized.as_bytes())?;
                if !headers.insert(header.clone()) {
                    anyhow::bail!("duplicate header dimension: {}", header.as_str());
                }
                dims.push(BatchKeyDimension::Header(header));
            }
            "query" => {
                let name = rest.trim();
                if name.is_empty() {
                    anyhow::bail!("query dimension requires a name");
                }
                if !queries.insert(name.to_string()) {
                    anyhow::bail!("duplicate query dimension: {name}");
                }
                dims.push(BatchKeyDimension::Query(name.to_string()));
            }
            other => anyhow::bail!("unsupported key dimension type: {other}"),
        }
    }

    Ok(dims)
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
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:my-fn
      x-smug:
        maxWaitMs: 10
        maxBatchSize: 16
        timeoutMs: 2000
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 2500).unwrap();
        let RouteMatch::Matched { op, path_params } =
            spec.match_request(&Method::GET, "/v1/items/123")
        else {
            panic!("expected route match");
        };

        assert_eq!(op.route_template, "/v1/items/{id}");
        assert_eq!(
            op.target_lambda,
            "arn:aws:lambda:us-east-1:123456789012:function:my-fn"
        );
        assert_eq!(op.max_wait_ms, 10);
        assert_eq!(op.max_batch_size, 16);
        assert_eq!(op.timeout_ms, 2000);
        assert_eq!(op.operation_id.as_deref(), Some("getItem"));
        assert_eq!(path_params.get("id").map(String::as_str), Some("123"));

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
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug: { maxWaitMs: 1, maxBatchSize: 1 }
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug: { maxWaitMs: 1, maxBatchSize: 1 }
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
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug: { maxWaitMs: 1, maxBatchSize: 0 }
"#;
        assert!(CompiledSpec::from_yaml_bytes(yaml, 1000).is_err());
    }

    #[test]
    fn invoke_mode_defaults_to_buffered() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: 1
        maxBatchSize: 1
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 1000).unwrap();
        let RouteMatch::Matched { op, .. } = spec.match_request(&Method::GET, "/x") else {
            panic!("expected match");
        };
        assert_eq!(op.invoke_mode, InvokeMode::Buffered);
    }

    #[test]
    fn parses_header_key_dimension() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: 1
        maxBatchSize: 1
        key:
          - header:X-Tenant-Id
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 1000).unwrap();
        let RouteMatch::Matched { op, .. } = spec.match_request(&Method::GET, "/x") else {
            panic!("expected match");
        };
        assert_eq!(op.key.len(), 1);
        assert_eq!(
            op.key,
            vec![BatchKeyDimension::Header(
                HeaderName::from_bytes(b"x-tenant-id").unwrap()
            )]
        );
    }

    #[test]
    fn rejects_invalid_key_dimensions() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: 1
        maxBatchSize: 1
        key: ["not-supported"]
"#;
        assert!(CompiledSpec::from_yaml_bytes(yaml, 1000).is_err());
    }

    #[test]
    fn rejects_duplicate_header_key_dimensions() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: 1
        maxBatchSize: 1
        key: ["header:x-tenant-id", "header:X-Tenant-Id"]
"#;
        assert!(CompiledSpec::from_yaml_bytes(yaml, 1000).is_err());
    }

    #[test]
    fn parses_query_key_dimension() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: 1
        maxBatchSize: 1
        key:
          - query:version
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 1000).unwrap();
        let RouteMatch::Matched { op, .. } = spec.match_request(&Method::GET, "/x") else {
            panic!("expected match");
        };
        assert_eq!(
            op.key,
            vec![BatchKeyDimension::Query("version".to_string())]
        );
    }

    #[test]
    fn rejects_duplicate_query_key_dimensions() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: 1
        maxBatchSize: 1
        key: ["query:version", "query:version"]
"#;
        assert!(CompiledSpec::from_yaml_bytes(yaml, 1000).is_err());
    }

    #[test]
    fn accepts_numeric_fields_as_strings() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: "25"
        maxBatchSize: "4"
        timeoutMs: "123"
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 1000).unwrap();
        let RouteMatch::Matched { op, .. } = spec.match_request(&Method::GET, "/x") else {
            panic!("expected match");
        };
        assert_eq!(op.max_wait_ms, 25);
        assert_eq!(op.max_batch_size, 4);
        assert_eq!(op.timeout_ms, 123);
    }

    #[test]
    fn parses_dynamic_wait_config() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: 100
        maxBatchSize: 2
        dynamicWait:
          minWaitMs: 1
          targetRps: 50
          steepness: 0.01
          samplingIntervalMs: 100
          smoothingSamples: 10
"#;
        let spec = CompiledSpec::from_yaml_bytes(yaml, 1000).unwrap();
        let RouteMatch::Matched { op, .. } = spec.match_request(&Method::GET, "/x") else {
            panic!("expected match");
        };

        let cfg = op.dynamic_wait.as_ref().expect("dynamic wait");
        assert_eq!(cfg.min_wait_ms, 1);
        assert_eq!(cfg.target_rps, 50.0);
        assert_eq!(cfg.steepness, 0.01);
        assert_eq!(cfg.sampling_interval_ms, 100);
        assert_eq!(cfg.smoothing_samples, 10);
    }

    #[test]
    fn dynamic_wait_min_must_be_lte_max() {
        let yaml = br#"
paths:
  /x:
    get:
      x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:fn
      x-smug:
        maxWaitMs: 10
        maxBatchSize: 2
        dynamicWait:
          minWaitMs: 11
"#;
        assert!(CompiledSpec::from_yaml_bytes(yaml, 1000).is_err());
    }
}
