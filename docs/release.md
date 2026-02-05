# Release workflow

This repo includes a GitHub Actions workflow to publish:

- the gateway container image to **public ECR**
- a new **Serverless Application Repository (SAR)** version of the bootstrap app

Workflow file: `.github/workflows/release-sar.yml`.

## Triggers

- Manual: **Actions → release-sar → Run workflow**
- Tags: pushing a tag like `v1.2.3` triggers the same workflow

## Required secrets

Configure these GitHub Actions secrets:

- `AWS_ROLE_TO_ASSUME`: IAM role ARN with permissions for ECR Public, S3, and SAR.
- `PUBLIC_ECR_ALIAS`: public ECR registry alias (e.g. `abc123xyz`).
- `PUBLIC_ECR_REPO`: repository name (e.g. `simple-multiplexer-gateway/gateway`).
- `SAR_ARTIFACTS_BUCKET`: S3 bucket used to store packaged SAR templates.
- `SAR_APPLICATION_ID`: SAR application ID to publish new versions under.

## What the workflow does

1. Resolves a version (workflow input → tag → `VERSION` file).
2. Builds and pushes the gateway image to public ECR.
3. Renders `bootstrap/template.yaml` with `DefaultGatewayImageIdentifier` set to that image URI.
4. Builds and packages the bootstrap app (includes the Mode A layer).
5. Publishes a new SAR application version with the packaged template.
6. Optionally sets SAR visibility to public.

## Permissions (high level)

The IAM role should allow:

- `ecr-public:*` (repository describe/create + push)
- `s3:PutObject` to the SAR artifacts bucket
- `serverlessrepo:CreateApplicationVersion` (and optionally `PutApplicationPolicy`)

## Notes

- The SAR version is pinned to the gateway image tag, so each SAR release maps to an exact image.
- To publish privately, keep the SAR app policy private and use a private ECR image instead.
