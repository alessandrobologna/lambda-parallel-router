# Bootstrap stack

This folder contains the account/region bootstrap stack used by Simple Multiplexer Gateway.

The bootstrap stack deploys shared resources:
- a shared S3 bucket for gateway config manifests (or uses an existing bucket)
- a CloudFormation macro (`SmugGateway`) that expands `Smug::Gateway::Service` into App Runner resources
- a custom resource handler (`Custom::SmugConfigPublisher`) used by the macro to publish config manifests
- a default gateway image identifier configured on the macro function and used when `ImageIdentifier` is empty or omitted
- the Mode A Runtime API proxy layer (arm64 + amd64) for layer proxy integrations

## Macro

The bootstrap stack registers a CloudFormation macro named `SmugGateway`.

To enable the macro, add `SmugGateway` to your template's `Transform` section, then declare one or
more `Smug::Gateway::Service` resources. The macro expands them into App Runner and supporting
resources.

## Resource type: `Smug::Gateway::Service`

### Syntax

```yaml
Gateway:
  Type: Smug::Gateway::Service
  Properties:
    # Required
    GatewayConfig: {}
    Spec: { paths: {} }

    # Optional
    ImageIdentifier: <ECR image identifier>
    ServiceName: <string>
    Port: 8080
    Environment: {}
    EnvironmentSecrets: {}
    AutoDeploymentsEnabled: true
    ConfigPrefix: <string>
    InstanceRoleArn: <string>
    InstanceConfiguration: {}
    AutoScalingConfiguration: {}
    AutoScalingConfigurationArn: <string>
    ObservabilityConfiguration: {}
```

### Properties

| Name | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `ImageIdentifier` | String | No | bootstrap default | ECR image identifier for the gateway. If omitted (or empty), the macro uses the default image configured by the bootstrap stack. |
| `GatewayConfig` | Object | Yes | - | Gateway settings object. Keys are PascalCase. See [GatewayConfig (manifest fields)](#gatewayconfig-manifest-fields). |
| `Spec` | Object | Yes | - | OpenAPI-like `paths` map. See [Spec object](#spec-object). |
| `ServiceName` | String | No | - | Sets the App Runner service name (`AWS::AppRunner::Service.Properties.ServiceName`). |
| `Port` | Integer | No | `8080` | Container port App Runner routes traffic to. Also used as the default for `GatewayConfig.ListenAddr` when omitted. |
| `Environment` | Object | No | `{}` | Map of environment variables for the gateway container. Values must resolve to strings (intrinsic functions are allowed). The macro always injects `SMUG_CONFIG_URI`, and defaults `AWS_REGION`, `AWS_DEFAULT_REGION`, and `RUST_LOG` if not provided. |
| `EnvironmentSecrets` | Object | No | `{}` | Map of environment variables sourced from Secrets Manager or SSM Parameter Store. Values must resolve to ARNs (intrinsic functions are allowed). These are emitted as `AWS::AppRunner::Service.SourceConfiguration.ImageRepository.ImageConfiguration.RuntimeEnvironmentSecrets`. |
| `AutoDeploymentsEnabled` | Boolean | No | `true` | Enables App Runner auto deployments for this service. See [Automatic deployments](#automatic-deployments). |
| `ConfigPrefix` | String | No | `smug/${AWS::StackName}/<LogicalId>/` | S3 prefix used for published manifests (must resolve to a string). The publisher writes `config/<sha256>.json` under this prefix. |
| `InstanceRoleArn` | String | No | - | Use an existing App Runner instance role (you own permissions). If omitted, the macro creates an instance role with S3 read + Lambda invoke permissions derived from the spec. |
| `InstanceConfiguration` | Object | No | - | Passed through to the service's `InstanceConfiguration` (e.g. `Cpu`, `Memory`). Some CPU/memory combinations are not supported (see App Runner docs). If it includes `InstanceRoleArn`, it must match `InstanceRoleArn` when both are set. |
| `AutoScalingConfiguration` | Object | No | - | Creates an `AWS::AppRunner::AutoScalingConfiguration` resource and wires it to the service. Mutually exclusive with `AutoScalingConfigurationArn`. |
| `AutoScalingConfigurationArn` | String | No | - | Uses an existing auto scaling configuration. Mutually exclusive with `AutoScalingConfiguration`. |
| `ObservabilityConfiguration` | Object | No | - | Enables tracing. This macro supports `AWSXRAY` (App Runner built-in X-Ray integration) and `OTEL` (direct OTLP export configured via env vars/secrets). See [ObservabilityConfiguration](#observabilityconfiguration). |

Notes:
- `PORT` is a reserved App Runner environment variable name. It can't be set in `Environment` or `EnvironmentSecrets`.
- If `ImageIdentifier` is omitted (or empty), the macro uses the default gateway image configured by the bootstrap stack.
- The macro configures the App Runner health check to use HTTP `GET /readyz`.
- X-Ray tracing requires application instrumentation and X-Ray permissions on the instance role.

### Return values

The macro replaces your `Smug::Gateway::Service` resource with an `AWS::AppRunner::Service` **using the same logical id**.

That means you can use the normal App Runner attributes/refs (for example `!GetAtt Gateway.ServiceUrl`).

### Example

Minimal example:

```yaml
Transform:
  - AWS::Serverless-2016-10-31
  - SmugGateway

Resources:
  Gateway:
    Type: Smug::Gateway::Service
    Properties:
      Port: 8080
      Environment:
        RUST_LOG: info
      GatewayConfig:
        # GatewayConfig keys are PascalCase.
        # ListenAddr defaults to 0.0.0.0:<Port> if omitted.
        DefaultTimeoutMs: 2000
      InstanceConfiguration:
        Cpu: "1 vCPU"
        Memory: "2 GB"
      AutoScalingConfiguration:
        AutoScalingConfigurationName: my-gateway-autoscaling
        MinSize: 2
        MaxSize: 4
        MaxConcurrency: 200
      Spec:
        openapi: 3.0.0
        paths:
          /hello:
            get:
              x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:my-fn
              x-smug:
                # x-smug keys are camelCase.
                maxWaitMs: 25
                maxBatchSize: 4
                invokeMode: buffered # buffered | response_stream
```

At deploy time, the macro expands this into:
- an `AWS::AppRunner::Service` (same logical id as your `Smug::Gateway::Service` resource)
- IAM roles for ECR access and instance permissions
- a `Custom::SmugConfigPublisher` resource that uploads a resolved config manifest (`GatewayConfig` + `Spec`) into S3

The macro derives the set of Lambda ARNs to allow from `Spec.paths.*.*.x-target-lambda` (which must
resolve to Lambda function ARNs). `x-target-lambda` may be a string ARN or an intrinsic function
object (e.g. `!GetAtt SomeFn.Arn`).

## Config manifest

The gateway consumes a single YAML/JSON document that embeds both gateway settings and the OpenAPI-ish
`Spec`. The macro publishes this manifest to S3 as JSON, and the gateway reads it at runtime via:

- `SMUG_CONFIG_URI` (set automatically by the macro)

`Spec` is required at runtime; the gateway exits on startup if it is missing.

Updates are content-addressed:
- the publisher canonicalizes the manifest as JSON and computes `sha256`
- the object key includes the hash (so it changes whenever you change `GatewayConfig` or `Spec`)
- the App Runner service updates its env var to the new URI, which forces a new deployment

## GatewayConfig (manifest fields)

`GatewayConfig` is the settings object embedded into the published manifest. Keys are **PascalCase**.

In the published manifest, `GatewayConfig` fields are **flattened at the top level** alongside the
top-level `Spec` key.

The macro publishes a manifest that looks like:

```json
{
  "ListenAddr": "0.0.0.0:8080",
  "DefaultTimeoutMs": 2000,
  "Spec": { "paths": { } }
}
```

Notes:
- For the macro flow, `ListenAddr` is optional because the publisher defaults it from `Port`.
- Numeric fields accept either numbers or numeric strings (e.g. `"2000"`).

| Name | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `ListenAddr` | String | Yes\* | - | Address to bind the HTTP server to (for example `0.0.0.0:8080`). \*Required if you run the gateway from a local manifest file; defaulted by the publisher when using the macro. |
| `AwsRegion` | String | No | - | Optional AWS region override for the Lambda client. If omitted, the AWS SDK resolves region from the environment. |
| `MaxInflightInvocations` | Integer | No | `64` | Maximum number of concurrent in-flight Lambda invocations across all routes. |
| `MaxInflightRequests` | Integer | No | `4096` | Maximum number of in-flight HTTP requests across all routes. When exceeded, the gateway rejects requests with HTTP 429. |
| `MaxPendingInvocations` | Integer | No | `256` | Maximum number of queued invocations waiting for execution. When full, the gateway rejects new batches with HTTP 429. |
| `MaxQueueDepthPerKey` | Integer | No | `1000` | Maximum queued requests per batch key. When full, new requests are rejected with HTTP 429. |
| `IdleTtlMs` | Integer | No | `30000` | Idle eviction TTL for per-key batching tasks. If a batch key sees no traffic for this long, its batching task is evicted. |
| `DefaultTimeoutMs` | Integer | No | `2000` | Default per-request timeout, used when an operation does not specify `x-smug.timeoutMs`. |
| `MaxBodyBytes` | Integer | No | `1048576` | Maximum accepted HTTP request body size. |
| `MaxInvokePayloadBytes` | Integer | No | `6291456` | Maximum JSON payload size sent to Lambda per invocation. If a batch exceeds this limit, the gateway splits it into multiple invocations when possible; otherwise affected requests fail. |
| `ForwardHeaders` | Object | No | `{}` | Header forwarding policy. See [ForwardHeaders object](#forwardheaders-object). |

### ForwardHeaders object

| Name | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `Allow` | List<String> | No | `[]` | If non-empty, only forward these headers (case-insensitive). |
| `Deny` | List<String> | No | `[]` | Always drop these headers (case-insensitive). |

Notes:
- Hop-by-hop headers are always dropped.
- Only headers that can be decoded as UTF-8 are forwarded.

## Spec object

The gateway accepts an OpenAPI-like document and uses only:

- `paths` (required): map of route templates to path items

Other OpenAPI fields (such as `openapi`, `info`, etc.) are allowed but ignored by the gateway.

### Spec shape

```yaml
Spec:
  openapi: 3.0.0 # ignored by gateway (allowed)
  info: {} # ignored by gateway (allowed)
  paths:
    /hello:
      get:
        operationId: hello # optional
        x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:my-fn
        x-smug:
          maxWaitMs: 25
          maxBatchSize: 4
```

### Route templates

Path keys must:
- start with `/`
- use OpenAPI-style `{param}` templates (for example `/v1/items/{id}`)
- have balanced braces (nested or unmatched braces are rejected)

Supported HTTP methods under a path item:
- `get`, `post`, `put`, `delete`, `patch`, `head`, `options`

### Operation fields

| Name | Type | Required | Description |
| --- | --- | --- | --- |
| `operationId` | String | No | Optional identifier for the operation (used for observability/debugging). |
| `x-target-lambda` | String | Yes | Target Lambda **function ARN** (may include a qualifier). Must resolve to a function ARN string at runtime. |
| `x-smug` | Object | Yes | Gateway-specific per-operation configuration. See [x-smug object](#x-smug-object). |

## x-smug object

Keys are **camelCase**.

| Name | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `maxWaitMs` | Integer | Yes | - | Maximum time to wait before flushing a batch (milliseconds). |
| `maxBatchSize` | Integer | Yes | - | Maximum number of requests per batch. Must be `> 0`. |
| `timeoutMs` | Integer | No | `GatewayConfig.DefaultTimeoutMs` | Per-request timeout override (milliseconds). |
| `invokeMode` | String | No | `buffered` | Lambda invoke mode. Allowed values: `buffered`, `response_stream`. |
| `key` | List<String> | No | `[]` | Optional additional batch key dimensions (see [Batch key dimensions](#batch-key-dimensions)). |
| `dynamicWait` | Object | No | - | Optional dynamic batching configuration (sigmoid-based). See [dynamicWait object](#dynamicwait-object). |

### Batch key dimensions

The gateway always partitions batches by `(x-target-lambda, method, route_template, invokeMode)`.

`key` lets you add extra dimensions to avoid mixing requests whose semantics differ (for example
multi-tenant traffic). Supported entries:

- `header:<name>` (case-insensitive; header name must be a valid HTTP header name)
- `query:<name>` (exact query parameter key)

Duplicates are rejected.

The following entries are accepted but ignored (because the gateway always keys by them anyway):
- `method`, `route`, `lambda`, `target_lambda`, `target-lambda`

### dynamicWait object

When `dynamicWait` is set, the gateway computes a per-batch flush window in `[minWaitMs, maxWaitMs]`
based on the request rate for the current batch key.

| Name | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `minWaitMs` | Integer | Yes | - | Lower bound for the computed wait window (milliseconds). Must be `<= maxWaitMs`. |
| `targetRps` | Number | No | `50` | Request rate (requests/sec) where the sigmoid is centered. Must be finite and `>= 0`. |
| `steepness` | Number | No | `0.01` | Sigmoid steepness around `targetRps`. Must be finite and `> 0`. |
| `samplingIntervalMs` | Integer | No | `100` | Sampling period (milliseconds). Must be `> 0`. |
| `smoothingSamples` | Integer | No | `10` | Moving average window size (number of samples). Must be `> 0`. |

## Automatic deployments

By default, the macro sets `AutoDeploymentsEnabled: true` on the App Runner service.

That means **pushing a new image** to the referenced ECR image/tag (e.g. `:latest`) can trigger an
immediate App Runner deployment via its auto-deploy pipeline. If you update the CloudFormation stack
at the same time, CloudFormation may fail with:

- `Service cannot be updated in the current state: OPERATION_IN_PROGRESS`

Options:
- Set `AutoDeploymentsEnabled: false` and let CloudFormation drive deployments.
- Use immutable tags (e.g. `:gitsha`) and update `ImageIdentifier` when deploying.
- If you keep auto-deploy enabled and mutable tags, deploy after pushes (or retry once the service is `RUNNING`).

## ObservabilityConfiguration

`ObservabilityConfiguration` enables trace export from the gateway.

### Syntax

```yaml
ObservabilityConfiguration:
  Vendor: AWSXRAY # AWSXRAY | OTEL

  # Required when Vendor is OTEL.
  OpentelemetryConfiguration:
    TracesEndpoint: https://ingest.vendor.example/v1/traces
    Protocol: http/protobuf # http/protobuf | grpc
    HeadersSecretArn: arn:aws:secretsmanager:us-east-1:123456789012:secret:otel-headers
```

### Behavior

`Vendor: AWSXRAY`
  - Creates an `AWS::AppRunner::ObservabilityConfiguration` and associates it with the service.
  - Defaults the gateway OTLP settings for App Runner X-Ray integration:
    - `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317`
    - `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=grpc`
    - `OTEL_EXPORTER_OTLP_INSECURE=true`
    - `OTEL_PROPAGATORS=xray,tracecontext,baggage`
    - `OTEL_METRICS_EXPORTER=none`
    - `SMUG_OBSERVABILITY_VENDOR=AWSXRAY`

`Vendor: OTEL`
  - Does not create an App Runner observability configuration (App Runner built-in tracing is X-Ray only).
  - Configures the gateway exporter:
    - `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=<TracesEndpoint>`
    - `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=<Protocol>` (defaults to `http/protobuf`)
    - `OTEL_PROPAGATORS=xray,tracecontext,baggage`
    - `OTEL_METRICS_EXPORTER=none`
    - `SMUG_OBSERVABILITY_VENDOR=OTEL`
  - If `HeadersSecretArn` is set, the macro injects the referenced secret as `SMUG_OTEL_HEADERS_JSON` using App Runner `RuntimeEnvironmentSecrets`.
    The gateway expects a JSON object whose keys are HTTP header names and values are header values.

Notes:
  - When `ServiceName` is set, the macro defaults `OTEL_SERVICE_NAME` to the same value.
  - `Environment` and `EnvironmentSecrets` override macro defaults.

## Permissions model

If you do not set `InstanceRoleArn`, the macro creates an App Runner instance role with:
- `s3:GetObject` on the generated prefix for the published manifest
- `lambda:InvokeFunction` and `lambda:InvokeWithResponseStream` on every `x-target-lambda` ARN found in the `Spec`
- `AWSXRayDaemonWriteAccess` when `ObservabilityConfiguration.Vendor` is `AWSXRAY`
- `secretsmanager:GetSecretValue` and/or `ssm:GetParameters` when `EnvironmentSecrets` is set
- `secretsmanager:GetSecretValue` for `ObservabilityConfiguration.OpentelemetryConfiguration.HeadersSecretArn` when provided

If you set `InstanceRoleArn`, you must provide equivalent permissions yourself.

## Generated logical IDs

For a resource with logical id `Gateway`, the macro may create additional resources named:
- `GatewaySmugConfigPublisher`
- `GatewaySmugEcrAccessRole`
- `GatewaySmugInstanceRole` (unless you set `InstanceRoleArn`)
- `GatewaySmugAutoScaling` (when `AutoScalingConfiguration` is provided)
- `GatewaySmugObservability` (when `ObservabilityConfiguration` is provided)

Do not declare resources with those logical IDs in the same template.

## Resource type: `Custom::SmugConfigPublisher`

The macro uses this custom resource to publish the config manifest to S3. You can also use it
directly if you do not want to use the macro.

### Syntax

```yaml
PublishConfig:
  Type: Custom::SmugConfigPublisher
  Properties:
    ServiceToken: <Lambda ARN from bootstrap output>
    BucketName: <string> # optional
    Prefix: <string> # optional
    Port: 8080 # optional
    GatewayConfig: {}
    Spec: { paths: {} }
```

### Properties

| Name | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `ServiceToken` | String | Yes | - | The `ConfigPublisherServiceToken` output from the bootstrap stack. |
| `BucketName` | String | No | `SMUG_DEFAULT_BUCKET` | S3 bucket where the manifest is uploaded. If omitted, the Lambda env var `SMUG_DEFAULT_BUCKET` must be set. |
| `Prefix` | String | No | `smug/` | S3 key prefix (normalized to end with `/`). |
| `Port` | Integer | No | `8080` | Used to default `ListenAddr` when omitted from `GatewayConfig`. |
| `GatewayConfig` | Object | Yes | - | GatewayConfig object (PascalCase keys). |
| `Spec` | Object | Yes | - | Spec object (OpenAPI-like `paths` map). |

### Return values

The custom resource returns these attributes (accessible via `!GetAtt PublishConfig.<Name>`):

| Name | Type | Description |
| --- | --- | --- |
| `BucketName` | String | Bucket used for the upload. |
| `Prefix` | String | Normalized prefix used for the upload. |
| `ConfigKey` | String | Object key for the uploaded manifest (includes `sha256`). |
| `ConfigS3Uri` | String | `s3://bucket/key` URI for the uploaded manifest. |
| `ConfigSha256` | String | SHA-256 of the canonical JSON manifest. |

## Deploy

Create a new config bucket (default name: `smug-config-<account>-<region>`):

```bash
sam deploy \
  --template-file bootstrap/template.yaml \
  --stack-name smug-bootstrap \
  --capabilities CAPABILITY_IAM
```

Set the default gateway image used by the macro (for services that omit `ImageIdentifier`):

```bash
sam deploy \
  --template-file bootstrap/template.yaml \
  --stack-name smug-bootstrap \
  --capabilities CAPABILITY_IAM \
  --parameter-overrides \
    DefaultGatewayRepositoryName=simple-multiplexer-gateway/gateway \
    DefaultGatewayImageTag=0.0.0
```

Use an existing bucket (leave it managed by you):

```bash
sam deploy \
  --template-file bootstrap/template.yaml \
  --stack-name smug-bootstrap \
  --capabilities CAPABILITY_IAM \
  --parameter-overrides UseExistingBucket=my-existing-bucket
```

Override the default gateway image identifier (for example, to point to a public ECR image):

```bash
sam deploy \
  --template-file bootstrap/template.yaml \
  --stack-name smug-bootstrap \
  --capabilities CAPABILITY_IAM \
  --parameter-overrides DefaultGatewayImageIdentifier=public.ecr.aws/your-alias/simple-multiplexer-gateway/gateway:1.2.3
```

## Outputs

- `ConfigBucketName`: bucket where gateway config manifests are stored.
- `ConfigPublisherServiceToken`: Lambda ARN to use as the `ServiceToken` for a `Custom::SmugConfigPublisher` resource.
- `LayerArm64Arn`: ARN of the arm64 runtime API proxy layer.
- `LayerAmd64Arn`: ARN of the amd64 runtime API proxy layer.
