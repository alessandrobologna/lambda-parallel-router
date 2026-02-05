import hashlib
import json
import logging
import os
import traceback
import urllib.request
from typing import Any, Dict, Mapping, Optional, Tuple

try:
    import boto3
except ImportError:  # pragma: no cover
    boto3 = None  # type: ignore[assignment]


logger = logging.getLogger(__name__)
logger.setLevel(os.environ.get("LOG_LEVEL", "INFO").upper())

def _sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _canonical_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _normalize_prefix(prefix: str) -> str:
    prefix = prefix.strip()
    if not prefix:
        return ""
    if prefix.startswith("/"):
        prefix = prefix[1:]
    if prefix and not prefix.endswith("/"):
        prefix += "/"
    return prefix


def _parse_port(value: Any) -> int:
    if value is None:
        return 8080
    if isinstance(value, bool):
        raise ValueError("Port must be a number or string.")
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    if isinstance(value, str):
        value = value.strip()
        if not value:
            return 8080
        try:
            return int(value)
        except ValueError as exc:
            raise ValueError("Port must be an integer string.") from exc
    raise ValueError("Port must be a number or string.")


def _cfn_send_response(
    event: Mapping[str, Any],
    *,
    status: str,
    reason: Optional[str],
    physical_resource_id: str,
    data: Mapping[str, Any],
) -> None:
    response_url = event["ResponseURL"]
    response_body = json.dumps(
        {
            "Status": status,
            "Reason": reason
            or f"See the details in CloudWatch Log Stream: {os.environ.get('AWS_LAMBDA_LOG_STREAM_NAME')}",
            "PhysicalResourceId": physical_resource_id,
            "StackId": event["StackId"],
            "RequestId": event["RequestId"],
            "LogicalResourceId": event["LogicalResourceId"],
            "NoEcho": False,
            "Data": data,
        }
    ).encode("utf-8")

    req = urllib.request.Request(response_url, data=response_body, method="PUT")
    req.add_header("Content-Type", "")
    req.add_header("Content-Length", str(len(response_body)))

    with urllib.request.urlopen(req) as resp:
        resp.read()


def _publish(
    *,
    bucket: str,
    prefix: str,
    gateway_config: Mapping[str, Any],
    spec: Mapping[str, Any],
    port: int,
) -> Tuple[Dict[str, Any], str]:
    if boto3 is None:
        raise RuntimeError("boto3 is required in the Lambda runtime.")

    prefix = _normalize_prefix(prefix)
    s3 = boto3.client("s3")

    config_obj = dict(gateway_config)
    config_obj.setdefault("ListenAddr", f"0.0.0.0:{port}")
    # The gateway expects the OpenAPI-ish spec to be embedded directly in the config manifest.
    config_obj["Spec"] = spec

    config_json = _canonical_json(config_obj)
    config_sha256 = _sha256_hex(config_json)
    config_key = f"{prefix}config/{config_sha256}.json"
    config_body = config_json.encode("utf-8")
    config_s3_uri = f"s3://{bucket}/{config_key}"

    logger.info(
        "Publishing config to s3://%s/%s (%d bytes)", bucket, config_key, len(config_body)
    )
    s3.put_object(
        Bucket=bucket,
        Key=config_key,
        Body=config_body,
        ContentType="application/json",
        Metadata={"smug-sha256": config_sha256, "smug-name": "config"},
    )

    data: Dict[str, Any] = {
        "BucketName": bucket,
        "Prefix": prefix,
        "ConfigKey": config_key,
        "ConfigS3Uri": config_s3_uri,
        "ConfigSha256": config_sha256,
    }

    physical_resource_id = f"smug-config-publisher:{bucket}:{prefix or '-'}"
    return data, physical_resource_id


def handler(event: Mapping[str, Any], context: Any) -> None:
    """
    CloudFormation custom resource handler.

    Properties:
      - BucketName (optional): target S3 bucket (defaults to env SMUG_DEFAULT_BUCKET)
      - Prefix (optional): object key prefix (default: "smug/")
      - Port (optional): port number used to default `ListenAddr` when omitted in `GatewayConfig`
      - GatewayConfig: object (YAML/JSON object)
      - Spec: object (YAML/JSON object)
    """
    logger.info("RequestType=%s LogicalResourceId=%s", event.get("RequestType"), event.get("LogicalResourceId"))

    request_type = event.get("RequestType")
    props = event.get("ResourceProperties") or {}

    physical_resource_id = event.get("PhysicalResourceId") or "smug-config-publisher"

    try:
        if request_type == "Delete":
            _cfn_send_response(
                event,
                status="SUCCESS",
                reason=None,
                physical_resource_id=physical_resource_id,
                data={},
            )
            return

        bucket = props.get("BucketName") or os.environ.get("SMUG_DEFAULT_BUCKET")
        if not isinstance(bucket, str) or not bucket:
            raise ValueError(
                "BucketName is required (or set SMUG_DEFAULT_BUCKET on the function)."
            )
        prefix = props.get("Prefix", "smug/")
        if not isinstance(prefix, str):
            raise ValueError("Prefix must be a string.")

        gateway_config = props.get("GatewayConfig")
        if not isinstance(gateway_config, dict):
            raise ValueError("GatewayConfig is required and must be an object.")

        spec = props.get("Spec")
        if not isinstance(spec, dict):
            raise ValueError("Spec is required and must be an object.")

        port = _parse_port(props.get("Port"))

        data, physical_resource_id = _publish(
            bucket=bucket,
            prefix=prefix,
            gateway_config=gateway_config,
            spec=spec,
            port=port,
        )

        _cfn_send_response(
            event,
            status="SUCCESS",
            reason=None,
            physical_resource_id=physical_resource_id,
            data=data,
        )
    except Exception as exc:
        logger.error("Failed: %s", exc)
        logger.error(traceback.format_exc())
        _cfn_send_response(
            event,
            status="FAILED",
            reason=str(exc),
            physical_resource_id=physical_resource_id,
            data={},
        )
