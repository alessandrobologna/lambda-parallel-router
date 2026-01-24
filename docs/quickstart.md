# Quickstart

This quickstart uses the SAM demo stack to deploy the router and sample Lambda functions. It is the fastest way to see batching behavior end to end.

## Prerequisites

- AWS credentials and a default region.
- `sam`, `aws`, and Docker.
- `make` (optional, for the repo helper targets).

See [sam/README.md](../sam/README.md) for the full deployment workflow and prerequisites.

## 1) Deploy the demo stack

From the repository root:

```bash
make deploy
```

This deploys the router service and sample Lambdas via SAM. Stack outputs include route URLs.

## 2) Find the route URLs

Find outputs in the CloudFormation console, or use the CLI:

```bash
aws cloudformation describe-stacks \
  --stack-name lambda-parallel-router-demo \
  --query 'Stacks[0].Outputs'
```

Look for outputs such as:

- `StreamingSimpleItemUrl`
- `StreamingDynamicItemUrl`
- `StreamingAdapterItemUrl`
- `StreamingModeALayerProxyNodeItemUrl`
- `DirectItemUrl`

Optional helper for local use:

```bash
export StreamingSimpleItemUrl="$(aws cloudformation describe-stacks \
  --stack-name lambda-parallel-router-demo \
  --query 'Stacks[0].Outputs[?OutputKey==`StreamingSimpleItemUrl`].OutputValue' \
  --output text)"
```

## 3) Send a request

Use the DynamoDB-backed `/item/{id}` routes for realistic workload behavior.

```bash
curl -sS "${StreamingSimpleItemUrl}?max-delay=0"
```

To vary the key, replace the final path segment (for example `hello`) with a numeric id:

```bash
curl -sS "https://.../streaming/simple/item/42?max-delay=0"
```

`max-delay=0` disables artificial sleep in the demo handlers. Remove it to see wait behavior under load.

## 4) Try a different integration mode

Adapter (Mode B) is the recommended default. See [docs/integrations.md](integrations.md) for details.

```javascript
const { batchAdapter } = require("lpr-lambda-adapter");
exports.handler = batchAdapter(handler);
```

## 5) Optional: run the router locally

Update [`examples/local/router.yaml`](../examples/local/router.yaml) with a real Lambda ARN, then run:

```bash
cargo run -p lpr-router -- --config examples/local/router.yaml
```

## 6) Optional: layer proxy (Mode A)

Mode A uses a Lambda Layer and exec wrapper and is experimental. See [docs/integrations.md](integrations.md) for setup and limitations.

```bash
AWS_LAMBDA_EXEC_WRAPPER=/opt/lpr/exec-wrapper.sh
LPR_MAX_CONCURRENCY=4
```
