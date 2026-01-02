# App Runner demo deployment (AWS SAM)

This folder contains an AWS SAM template that deploys:

- Two sample Lambda functions that implement the router batch contract:
  - buffered (`/hello`)
  - response streaming via NDJSON (`/hello-stream`)
- IAM roles for App Runner
- An App Runner service that runs the router container (ECR image)

The router image includes `sam/router/spec.yaml`, which references deployed Lambda ARNs via
environment variables injected by the App Runner service.

## Deploy

The SAM template expects the router image to already exist in ECR and is passed via the
`RouterImageIdentifier` parameter.

### Recommended: `make deploy` (repo root)

```bash
make deploy
```

This runs:
1) create/update an ECR repository creation template (CREATE_ON_PUSH)
2) docker login to ECR
3) build + push the router image (`Dockerfile.router`)
4) `sam build` + `sam deploy` with `RouterImageIdentifier=...`

Common overrides:

```bash
make deploy AWS_REGION=us-east-1 STACK_NAME=lambda-parallel-router-demo
make deploy ROUTER_REPO_PREFIX=lambda-parallel-router ROUTER_REPO_NAME=lambda-parallel-router/router ROUTER_IMAGE_TAG=latest
make deploy ROUTER_IMAGE_PLATFORM=linux/amd64
```

Notes:
- Repository creation templates are account+region scoped and are **not** managed by the stack.
  Use `make ecr-template-delete` if you want to remove the template.
- The Makefile defaults to building `linux/amd64` images. This matches the App Runner runtime in this demo.

## Try it

- Buffered route: `GET {RouterServiceUrl}/hello`
- Streaming route: `GET {RouterServiceUrl}/hello-stream?sleep_ms=250`
