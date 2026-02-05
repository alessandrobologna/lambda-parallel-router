# smug-runtime-api-proxy

Rust Lambda Extension that proxies the Lambda Runtime API.

Mode A uses the proxy to split one outer batch invocation into multiple virtual invocations
served to a managed runtime worker pool.

Docs:

- [docs/integrations.md](../../docs/integrations.md) (Layer Proxy (Mode A) setup and caveats)

## Build and publish the layer (SAM)

The layer contents map to `/opt` in Lambda.

- Extension binary: `/opt/extensions/smug-runtime-api-proxy`
- Exec wrapper: `/opt/smug/exec-wrapper.sh`

Build:

```bash
cd bootstrap
sam build --template template.yaml
```

Publish:

```bash
cd bootstrap
sam deploy --guided
```

Then configure the function to use the exec wrapper:

```bash
AWS_LAMBDA_EXEC_WRAPPER=/opt/smug/exec-wrapper.sh
```

Optionally set the runtime worker concurrency (recommended: match your route `maxBatchSize`):

```bash
SMUG_MAX_CONCURRENCY=4
```

Note: Python 3.14 concurrency remains experimental. As of `python:3.14.v32`, enabling
`AWS_LAMBDA_MAX_CONCURRENCY>1` can crash during init when `_LAMBDA_TELEMETRY_LOG_FD` is set
(awslambdaric log sink). The exec wrapper includes a best-effort workaround (it unsets
`_LAMBDA_TELEMETRY_LOG_FD` when concurrency is enabled).
