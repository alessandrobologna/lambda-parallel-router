# App Runner demo deployment (AWS SAM)

This folder contains an AWS SAM template that deploys:

- Nine sample Lambda functions that implement the gateway batch contract:
  - buffering/simple (`/buffering/simple/{greeting}`)
  - buffering/dynamic (`/buffering/dynamic/{greeting}`)
  - streaming/simple (`/streaming/simple/{greeting}`)
  - streaming/dynamic (`/streaming/dynamic/{greeting}`)
  - buffering/adapter (`/buffering/adapter/{greeting}`)
  - streaming/adapter (`/streaming/adapter/{greeting}`)
  - streaming/adapter SSE (`/streaming/adapter/sse`)
  - streaming/mode-a/python (`/streaming/mode-a/python/{greeting}`)
  - streaming/mode-a/node (`/streaming/mode-a/node/{greeting}`)
- An App Runner service that runs the gateway container (ECR image)

The gateway service definition is intentionally concise and is expanded by the `SmugGateway`
CloudFormation macro (see `bootstrap/`).

At deploy time, the macro publishes the inline `GatewayConfig` + `Spec` documents to S3 and sets an
environment variable on the App Runner service pointing the gateway to the generated `s3://...`
config URI.

## Prerequisite: bootstrap stack

Deploy the bootstrap stack once per account+region:

```bash
sam deploy \
  --template-file bootstrap/template.yaml \
  --stack-name smug-bootstrap \
  --capabilities CAPABILITY_IAM
```

## Deploy

The gateway image must already exist in ECR.

The gateway image is the default configured in the bootstrap stack.

### Recommended: `make deploy` (repo root)

```bash
make deploy
```

This runs:
1) create/update an ECR repository creation template (CREATE_ON_PUSH)
2) docker login to ECR
3) build + push the gateway image (`Dockerfile.gateway`)
4) `sam build` + `sam deploy` (deploys the built template in `.aws-sam/build/`)

Edit `sam/samconfig.toml` to change the stack name, region, or SAM deploy parameters.

Common overrides:

```bash
make deploy GATEWAY_REPO_PREFIX=simple-multiplexer-gateway GATEWAY_REPO_NAME=simple-multiplexer-gateway/gateway GATEWAY_IMAGE_TAG=0.0.0
make deploy GATEWAY_IMAGE_PLATFORM=linux/amd64
```

Notes:
- Repository creation templates are account+region scoped and are **not** managed by the stack.
  Use `make ecr-template-delete` if you want to remove the template.
- The Makefile defaults to building `linux/amd64` images. This matches the App Runner runtime in this demo.

## Try it

The legacy routes use `{greeting}`. For DynamoDB-backed testing, prefer the `/item/{id}` routes.

- Buffering simple: `GET {GatewayServiceUrl}/buffering/simple/hello?max-delay=0`
- Buffering dynamic: `GET {GatewayServiceUrl}/buffering/dynamic/hello?max-delay=0`
- Streaming simple: `GET {GatewayServiceUrl}/streaming/simple/hello?max-delay=0`
- Streaming dynamic: `GET {GatewayServiceUrl}/streaming/dynamic/hello?max-delay=0`
- Buffering adapter: `GET {GatewayServiceUrl}/buffering/adapter/hello?max-delay=0`
- Streaming adapter: `GET {GatewayServiceUrl}/streaming/adapter/hello?max-delay=0`
- Streaming adapter SSE: `GET {GatewayServiceUrl}/streaming/adapter/sse?max-delay=0`
- Mode A Python: `GET {GatewayServiceUrl}/streaming/mode-a/python/hello?max-delay=0`
- Mode A Node: `GET {GatewayServiceUrl}/streaming/mode-a/node/hello?max-delay=0`
- Direct (Lambda Function URL): `GET {DirectHelloUrl}hello?max-delay=0`

For load testing, use a numeric item id (e.g. `0..999`) to hit different DynamoDB keys:

- `GET {GatewayServiceUrl}/streaming/simple/item/42?max-delay=0`
- `GET {GatewayServiceUrl}/streaming/dynamic/item/42?max-delay=0`
- `GET {GatewayServiceUrl}/streaming/adapter/item/42?max-delay=0`
- `GET {GatewayServiceUrl}/streaming/mode-a/node/item/42?max-delay=0`
- `GET {DirectHelloUrl}item/42?max-delay=0`

## Load testing (k6)

The repo includes a simple k6 + Python driver that reads the route URLs from the stack outputs and
produces CSV + charts.

Prereqs:
- `k6`
- `uv` (or install the Python deps in `benchmark/benchmark.py` manually)

Run:

```bash
uv run benchmark/benchmark.py \
  --stack simple-multiplexer-gateway-demo \
  --region us-east-1 \
  --ramp-duration 3m \
  --hold-duration 30s \
  --stage-targets 50,100,150 \
  --max-delay-ms 0
```

Outputs are written to `benchmark-results/` (CSV + summary + charts).

Note: The demo gateway service enables `SMUG_INCLUDE_BATCH_SIZE_HEADER=1`, which adds the `x-smug-batch-size`
response header. The k6 script records it as `smug_batch_size` so the benchmark can estimate how many
Lambda invocations were used per endpoint.
