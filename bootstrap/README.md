# Bootstrap stack

This folder contains the account/region bootstrap resources used by `lambda-parallel-router`.

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

- `ConfigBucketName`: bucket where router config/spec objects are stored.
- `ConfigPublisherServiceToken`: Lambda ARN to use as the `ServiceToken` for a `Custom::LprConfigPublisher` resource.
