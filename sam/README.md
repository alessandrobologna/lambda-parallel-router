# App Runner demo deployment (AWS SAM)

This folder contains an AWS SAM template that deploys:

- Four sample Lambda functions that implement the router batch contract:
  - buffering/simple (`/buffering/simple/hello`)
  - buffering/adaptive (`/buffering/adaptive/hello`)
  - streaming/simple (`/streaming/simple/hello`)
  - streaming/adaptive (`/streaming/adaptive/hello`)
- An App Runner service that runs the router container (ECR image)

The router service definition is intentionally concise and is expanded by the `LprRouter`
CloudFormation macro (see `bootstrap/`).

At deploy time, the macro publishes the inline `RouterConfig` + `Spec` documents to S3 and sets an
environment variable on the App Runner service pointing the router to the generated `s3://...`
config URI.

## Prerequisite: bootstrap stack

Deploy the bootstrap stack once per account+region:

```bash
sam deploy \
  --template-file bootstrap/template.yaml \
  --stack-name lpr-bootstrap \
  --capabilities CAPABILITY_IAM
```

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
4) `sam build` + `sam deploy` with `RouterImageIdentifier=...` (deploys the built template in `.aws-sam/build/`)

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

- Buffering simple: `GET {RouterServiceUrl}/buffering/simple/hello?max-delay=250`
- Buffering adaptive: `GET {RouterServiceUrl}/buffering/adaptive/hello?max-delay=250`
- Streaming simple: `GET {RouterServiceUrl}/streaming/simple/hello?max-delay=250`
- Streaming adaptive: `GET {RouterServiceUrl}/streaming/adaptive/hello?max-delay=250`
