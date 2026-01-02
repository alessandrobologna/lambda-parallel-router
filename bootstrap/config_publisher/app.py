import hashlib
import json
import logging
import os
import re
import traceback
import urllib.request
from dataclasses import dataclass
from typing import Any, Dict, List, Mapping, Optional, Tuple

try:
    import boto3
except ImportError:  # pragma: no cover
    boto3 = None  # type: ignore[assignment]


logger = logging.getLogger(__name__)
logger.setLevel(os.environ.get("LOG_LEVEL", "INFO").upper())

_NAME_RE = re.compile(r"^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$")


def _sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _normalize_prefix(prefix: str) -> str:
    prefix = prefix.strip()
    if not prefix:
        return ""
    if prefix.startswith("/"):
        prefix = prefix[1:]
    if prefix and not prefix.endswith("/"):
        prefix += "/"
    return prefix


def _validate_object_name(name: str) -> None:
    if not _NAME_RE.fullmatch(name):
        raise ValueError(
            "Invalid object name. Expected 1-128 chars matching "
            r"^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$"
        )


@dataclass(frozen=True)
class PublishObject:
    name: str
    content: str
    suffix: str = ".yaml"
    content_type: str = "text/yaml; charset=utf-8"


def _parse_objects(props: Mapping[str, Any]) -> List[PublishObject]:
    objects: List[PublishObject] = []

    if "Objects" in props and props["Objects"] is not None:
        raw_objects = props["Objects"]
        if not isinstance(raw_objects, list):
            raise ValueError("Objects must be a list.")
        for item in raw_objects:
            if not isinstance(item, dict):
                raise ValueError("Objects entries must be objects.")
            name = item.get("Name")
            content = item.get("Content")
            suffix = item.get("Suffix", ".yaml")
            content_type = item.get("ContentType", "text/yaml; charset=utf-8")
            if not isinstance(name, str) or not name:
                raise ValueError("Objects[].Name is required.")
            if not isinstance(content, str):
                raise ValueError("Objects[].Content must be a string.")
            if not isinstance(suffix, str) or not suffix:
                raise ValueError("Objects[].Suffix must be a non-empty string.")
            if not isinstance(content_type, str) or not content_type:
                raise ValueError("Objects[].ContentType must be a non-empty string.")
            objects.append(
                PublishObject(
                    name=name, content=content, suffix=suffix, content_type=content_type
                )
            )
        if not objects:
            raise ValueError("Objects must not be empty.")

    if not objects:
        config_yaml = props.get("ConfigYaml")
        if isinstance(config_yaml, str):
            objects.append(PublishObject(name="config", content=config_yaml))

        spec_yaml = props.get("SpecYaml")
        if isinstance(spec_yaml, str):
            objects.append(PublishObject(name="spec", content=spec_yaml))

    if not objects:
        raise ValueError(
            "No objects to publish. Provide Objects[] or (ConfigYaml and/or SpecYaml)."
        )

    for obj in objects:
        _validate_object_name(obj.name)

    return objects


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
    objects: List[PublishObject],
) -> Tuple[Dict[str, Any], str]:
    if boto3 is None:
        raise RuntimeError("boto3 is required in the Lambda runtime.")

    prefix = _normalize_prefix(prefix)
    s3 = boto3.client("s3")

    published: Dict[str, Any] = {}
    for obj in objects:
        sha256 = _sha256_hex(obj.content)
        key = f"{prefix}{obj.name}/{sha256}{obj.suffix}"
        body = obj.content.encode("utf-8")

        logger.info("Publishing %s to s3://%s/%s (%d bytes)", obj.name, bucket, key, len(body))
        s3.put_object(
            Bucket=bucket,
            Key=key,
            Body=body,
            ContentType=obj.content_type,
            Metadata={"lpr-sha256": sha256, "lpr-name": obj.name},
        )

        published[obj.name] = {
            "Key": key,
            "Sha256": sha256,
            "S3Uri": f"s3://{bucket}/{key}",
            "Bytes": len(body),
        }

    data: Dict[str, Any] = {"BucketName": bucket, "Prefix": prefix, "Published": published}

    if "config" in published:
        data["ConfigKey"] = published["config"]["Key"]
        data["ConfigS3Uri"] = published["config"]["S3Uri"]
        data["ConfigSha256"] = published["config"]["Sha256"]
    if "spec" in published:
        data["SpecKey"] = published["spec"]["Key"]
        data["SpecS3Uri"] = published["spec"]["S3Uri"]
        data["SpecSha256"] = published["spec"]["Sha256"]

    physical_resource_id = f"lpr-config-publisher:{bucket}:{prefix or '-'}"
    return data, physical_resource_id


def handler(event: Mapping[str, Any], context: Any) -> None:
    """
    CloudFormation custom resource handler.

    Properties:
      - BucketName (optional): target S3 bucket (defaults to env LPR_DEFAULT_BUCKET)
      - Prefix (optional): object key prefix (default: "lpr/")
      - Objects: [{ Name, Content, Suffix?, ContentType? }]
      - ConfigYaml / SpecYaml convenience properties (used only when Objects is omitted)
    """

    logger.info("Event: %s", json.dumps(event))

    request_type = event.get("RequestType")
    props = event.get("ResourceProperties") or {}

    physical_resource_id = event.get("PhysicalResourceId") or "lpr-config-publisher"

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

        bucket = props.get("BucketName") or os.environ.get("LPR_DEFAULT_BUCKET")
        if not isinstance(bucket, str) or not bucket:
            raise ValueError(
                "BucketName is required (or set LPR_DEFAULT_BUCKET on the function)."
            )
        prefix = props.get("Prefix", "lpr/")
        if not isinstance(prefix, str):
            raise ValueError("Prefix must be a string.")

        objects = _parse_objects(props)
        data, physical_resource_id = _publish(
            bucket=bucket,
            prefix=prefix,
            objects=objects,
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
