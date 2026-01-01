# App Runner demo deployment (AWS SAM)

This folder contains an AWS SAM template that deploys:

- Two sample Lambda functions that implement the router batch contract:
  - buffered (`/hello`)
  - response streaming via NDJSON (`/hello-stream`)
- An ECR repository for the router container image
- An (optional) App Runner service that runs the router container

The router image includes `sam/router/spec.yaml`, which references deployed Lambda ARNs via
environment variables injected by the App Runner service.

## Deploy

1) Deploy the stack **without** creating the App Runner service yet (creates Lambdas + ECR repo + roles):

```bash
sam build --template-file sam/template.yaml
sam deploy --guided --template-file sam/template.yaml
```

Keep `CreateRouterService` set to `false`.

2) Build + push the router container image to the created ECR repo:

```bash
# Get `RouterEcrRepositoryUri` from stack outputs, then:
aws ecr get-login-password | docker login --username AWS --password-stdin "$(cut -d/ -f1 <<<"$REPO_URI")"
docker build -f Dockerfile.router -t "$REPO_URI:latest" .
docker push "$REPO_URI:latest"
```

3) Update the stack to create the App Runner service:

```bash
sam deploy \
  --template-file sam/template.yaml \
  --parameter-overrides CreateRouterService=true RouterImageTag=latest
```

After completion, the stack output `RouterServiceUrl` is the router base URL.

## Try it

- Buffered route: `GET {RouterServiceUrl}/hello`
- Streaming route: `GET {RouterServiceUrl}/hello-stream?sleep_ms=250`

