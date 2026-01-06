# Bootstrap stack

This folder contains the account/region bootstrap resources used by `lambda-parallel-router`.

It deploys:
- a shared S3 bucket for router config manifests (or uses an existing bucket)
- a CloudFormation macro (`LprRouter`) that expands `Lpr::Router::Service` into App Runner resources
- a custom resource handler (`Custom::LprConfigPublisher`) used by the macro to publish config manifests

## Macro (recommended)

The bootstrap stack registers a CloudFormation macro named `LprRouter`.

With the macro enabled, templates can declare a simplified router resource:

```yaml
Transform:
  - AWS::Serverless-2016-10-31
  - LprRouter

Resources:
  Router:
    Type: Lpr::Router::Service
    Properties:
      ImageIdentifier: <your ECR image identifier>
      Port: 8080
      Environment:
        RUST_LOG: info
      RouterConfig:
        # RouterConfig keys are PascalCase.
        # ListenAddr defaults to 0.0.0.0:<Port> if omitted.
        DefaultTimeoutMs: 2000
      InstanceConfiguration:
        Cpu: "0.5 vCPU"
        # Memory defaults to 2 GB if omitted.
      AutoScalingConfiguration:
        AutoScalingConfigurationName: my-router-autoscaling
        MinSize: 2
        MaxSize: 4
        MaxConcurrency: 200
      Spec:
        openapi: 3.0.0
        paths:
          /hello:
            get:
              x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:my-fn
              x-lpr:
                # x-lpr keys are camelCase.
                maxWaitMs: 25
                maxBatchSize: 4
                invokeMode: buffered # buffered | response_stream
```

At deploy time, the macro expands this into:
- an `AWS::AppRunner::Service` (same logical id as your `Lpr::Router::Service` resource)
- IAM roles for ECR access and instance permissions
- a `Custom::LprConfigPublisher` resource that uploads a resolved config manifest (`RouterConfig` + `Spec`) into S3

The macro derives the set of Lambda ARNs to allow from `Spec.paths.*.*.x-target-lambda` (which must
resolve to Lambda function ARNs). `x-target-lambda` may be a string ARN or an intrinsic function
object (e.g. `!GetAtt SomeFn.Arn`).

## Config manifest

The router consumes a single YAML/JSON document that embeds both router settings and the OpenAPI-ish
`Spec`. The macro publishes this manifest to S3 as JSON, and the router reads it at runtime via:

- `LPR_CONFIG_URI` (set automatically by the macro)

Updates are content-addressed:
- the publisher canonicalizes the manifest as JSON and computes `sha256`
- the object key includes the hash (so it changes whenever you change `RouterConfig` or `Spec`)
- the App Runner service updates its env var to the new URI, which forces a new deployment

## Automatic deployments (important)

By default, the macro sets `AutoDeploymentsEnabled: true` on the App Runner service.

That means **pushing a new image** to the referenced ECR image/tag (e.g. `:latest`) can trigger an
immediate App Runner deployment via its auto-deploy pipeline. If you update the CloudFormation stack
at the same time, CloudFormation may fail with:

- `Service cannot be updated in the current state: OPERATION_IN_PROGRESS`

Options:
- Set `AutoDeploymentsEnabled: false` and let CloudFormation drive deployments.
- Use immutable tags (e.g. `:gitsha`) and update `ImageIdentifier` when deploying.
- If you keep auto-deploy enabled + mutable tags, deploy after pushes (or retry once the service is `RUNNING`).

## Permissions model

If you do not set `InstanceRoleArn`, the macro creates an App Runner instance role with:
- `s3:GetObject` on the generated prefix for the published manifest
- `lambda:InvokeFunction` and `lambda:InvokeWithResponseStream` on every `x-target-lambda` ARN found in the `Spec`

If you set `InstanceRoleArn`, you must provide equivalent permissions yourself.

## Generated logical IDs

For a resource with logical id `Router`, the macro may create additional resources named:
- `RouterLprConfigPublisher`
- `RouterLprEcrAccessRole`
- `RouterLprInstanceRole` (unless you set `InstanceRoleArn`)
- `RouterLprAutoScaling` (when `AutoScalingConfiguration` is provided)

Do not declare resources with those logical IDs in the same template.

Optional properties (passed through to App Runner):
- `ServiceName` (sets the App Runner service name)
- `ConfigPrefix` (S3 prefix for published manifests; default `lpr/${AWS::StackName}/${LogicalId}/`)
- `AutoDeploymentsEnabled` (default `true`)
- `InstanceConfiguration` (e.g. `Cpu`, `Memory`)
- `AutoScalingConfiguration` (creates an `AWS::AppRunner::AutoScalingConfiguration` and wires it to the service)
- `AutoScalingConfigurationArn` (use an existing auto scaling configuration)
- `InstanceRoleArn` (use an existing App Runner instance role; you own permissions)

`AutoScalingConfiguration` and `AutoScalingConfigurationArn` are mutually exclusive.

## Deploy

Create a new config bucket (default name: `lpr-config-<account>-<region>`):

```bash
sam deploy \
  --template-file bootstrap/template.yaml \
  --stack-name lpr-bootstrap \
  --capabilities CAPABILITY_IAM
```

Use an existing bucket (leave it managed by you):

```bash
sam deploy \
  --template-file bootstrap/template.yaml \
  --stack-name lpr-bootstrap \
  --capabilities CAPABILITY_IAM \
  --parameter-overrides UseExistingBucket=my-existing-bucket
```

## Outputs

- `ConfigBucketName`: bucket where router config manifests are stored.
- `ConfigPublisherServiceToken`: Lambda ARN to use as the `ServiceToken` for a `Custom::LprConfigPublisher` resource.
