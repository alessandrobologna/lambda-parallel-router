# App Runner demo deployment (AWS SAM)

This folder contains an AWS SAM template that deploys:

- Two sample Lambda functions that implement the router batch contract:
  - buffered (`/hello`)
  - response streaming via NDJSON (`/hello-stream`)
- An ECR **repository creation template** so the router image repository is created automatically
  on first push (create-on-push)
- An App Runner service that runs the router container

The router image includes `sam/router/spec.yaml`, which references deployed Lambda ARNs via
environment variables injected by the App Runner service.

## Deploy

The stack uses a CloudFormation custom resource to wait until the router image exists in ECR
before creating the App Runner service. This avoids a “deploy twice” workflow.

1) Start the deployment:

```bash
sam build --template-file sam/template.yaml
sam deploy --guided --template-file sam/template.yaml
```

CloudFormation will pause while waiting for the image tag defined by `RouterImageTag` (default:
`latest`) to exist.

2) In another terminal, build + push the router container image:

```bash
REGION="$(aws configure get region)"
ACCOUNT_ID="$(aws sts get-caller-identity --query Account --output text)"
REPO_NAME="lambda-parallel-router/router"  # same as RouterRepositoryName
TAG="latest" # same as RouterImageTag

REPO_URI="${ACCOUNT_ID}.dkr.ecr.${REGION}.amazonaws.com/${REPO_NAME}"

aws ecr get-login-password --region "$REGION" | docker login --username AWS --password-stdin "${ACCOUNT_ID}.dkr.ecr.${REGION}.amazonaws.com"
docker build -f Dockerfile.router -t "${REPO_URI}:${TAG}" .
docker push "${REPO_URI}:${TAG}"
```

Notes:
- Create-on-push requires a matching repository creation template prefix; the template in `sam/template.yaml`
  defaults to `RouterRepositoryPrefix=lambda-parallel-router`, so `RouterRepositoryName` should start with
  `lambda-parallel-router/`.
- If you push before the repository creation template exists, ECR may return “repository not found”.
  Wait a few seconds and retry the `docker push`.

3) Once the image is pushed, the stack should finish creating and output `RouterServiceUrl`.

## Try it

- Buffered route: `GET {RouterServiceUrl}/hello`
- Streaming route: `GET {RouterServiceUrl}/hello-stream?sleep_ms=250`
