# Bootstrap stack

This folder contains the account/region bootstrap resources used by `lambda-parallel-router`.

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
        ListenAddr: "0.0.0.0:8080"
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
        paths: {}
```

At deploy time, the macro expands this into:
- an `AWS::AppRunner::Service` (same logical id as your `Lpr::Router::Service` resource)
- IAM roles for ECR access and instance permissions
- a `Custom::LprConfigPublisher` resource that uploads the resolved config manifest (`RouterConfig` + `Spec`) into S3

The macro derives the set of Lambda ARNs to allow from `Spec.paths.*.*.x-target-lambda` (which must
resolve to Lambda function ARNs).

Optional properties (passed through to App Runner):
- `InstanceConfiguration` (e.g. `Cpu`, `Memory`)
- `AutoScalingConfiguration` (creates an `AWS::AppRunner::AutoScalingConfiguration` and wires it to the service)
- `AutoScalingConfigurationArn` (use an existing auto scaling configuration)

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
