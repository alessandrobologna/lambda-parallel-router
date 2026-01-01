use std::collections::{BTreeMap, HashMap};

use http::Method;
use matchit::Router;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvokeMode {
    Buffered,
    ResponseStream,
}

impl Default for InvokeMode {
    fn default() -> Self {
        Self::Buffered
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LprOperationConfig {
    pub max_wait_ms: u64,
    pub max_batch_size: usize,

    #[serde(default)]
    pub timeout_ms: Option<u64>,

    #[serde(default)]
    pub invoke_mode: InvokeMode,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Operation {
    #[serde(rename = "operationId", default)]
    pub operation_id: Option<String>,

    #[serde(rename = "x-target-lambda")]
    pub target_lambda: String,

    #[serde(rename = "x-lpr")]
    pub lpr: LprOperationConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
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
pub struct OpenApiLikeSpec {
    pub paths: BTreeMap<String, PathItem>,
}

#[derive(Debug, Clone)]
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
pub struct CompiledSpec {
    router: Router<RouteConfig>,
}

#[derive(Debug, Clone)]
pub enum RouteMatch<'a> {
    NotFound,
    MethodNotAllowed { allowed: Vec<Method> },
    Matched(&'a OperationConfig),
}

impl CompiledSpec {
    pub fn from_yaml_bytes(bytes: &[u8], default_timeout_ms: u64) -> anyhow::Result<Self> {
        let spec: OpenApiLikeSpec = serde_yaml::from_slice(bytes)?;
        Self::compile(spec, default_timeout_ms)
    }

    pub fn match_request<'a>(&'a self, method: &Method, path: &str) -> RouteMatch<'a> {
        let Ok(matched) = self.router.at(path) else {
            return RouteMatch::NotFound;
        };

        match matched.value.ops_by_method.get(method) {
            Some(op) => RouteMatch::Matched(op),
            None => RouteMatch::MethodNotAllowed {
                allowed: matched.value.ops_by_method.keys().cloned().collect(),
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
}
