# lpr-runtime-api-proxy

Rust Lambda Extension that proxies the Lambda Runtime API.

Mode A uses the proxy to split one outer batch invocation into multiple virtual invocations
served to a managed runtime worker pool.

Docs:

- `docs/MODE_A_LAYER_PROXY_IMPLEMENTATION_PLAN.md`

## Build and publish the layer (SAM)

The layer contents map to `/opt` in Lambda.

- Extension binary: `/opt/extensions/lpr-runtime-api-proxy`
- Exec wrapper: `/opt/lpr/exec-wrapper.sh`

Build:

```bash
cd lambda-kit/layer-proxy
sam build --template template.yaml
```

Publish:

```bash
cd lambda-kit/layer-proxy
sam deploy --guided
```

Then configure the function to use the exec wrapper:

```bash
AWS_LAMBDA_EXEC_WRAPPER=/opt/lpr/exec-wrapper.sh
```
