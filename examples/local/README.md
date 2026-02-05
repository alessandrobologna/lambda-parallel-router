# Local example

This directory contains a gateway config manifest for running the gateway locally and invoking AWS Lambda.

## Prerequisites

- Rust toolchain (`cargo`)
- AWS credentials that can invoke the target Lambda function
- A Lambda function that implements the gateway contract (buffered or streaming)

## Configure

Edit `examples/local/gateway.yaml`.

Update these fields:

- `AwsRegion`: must match the region of the target Lambda function.
- `Spec.paths.*.*.x-target-lambda`: set this to a Lambda function ARN (optionally with a qualifier).

Example:

```yaml
Spec:
  paths:
    /hello:
      get:
        x-target-lambda: arn:aws:lambda:us-east-1:123456789012:function:my-fn
```

## Run

```bash
cargo run -p smug-gateway -- --config examples/local/gateway.yaml
```

## Try it

```bash
curl -sS -D - http://127.0.0.1:3000/hello
```

If `invokeMode` is set to `response_stream`, use `curl -N`:

```bash
curl -sS -N http://127.0.0.1:3000/hello
```

## Troubleshooting

- `AccessDeniedException`: the caller does not have permission to invoke the Lambda function.
- `ResourceNotFoundException`: the ARN or region is incorrect.
