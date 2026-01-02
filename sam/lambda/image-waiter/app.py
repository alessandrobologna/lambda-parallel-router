import json
import logging
import time
import urllib.error
import urllib.request

import boto3
from botocore.exceptions import ClientError

logger = logging.getLogger(__name__)
logger.setLevel(logging.INFO)


def _send_cfn_response(
    *,
    event: dict,
    context,
    status: str,
    data: dict | None = None,
    physical_resource_id: str | None = None,
    reason: str | None = None,
) -> None:
    response_url = event["ResponseURL"]
    response_body = {
        "Status": status,
        "Reason": reason
        or f"See the details in CloudWatch Log Stream: {context.log_stream_name}",
        "PhysicalResourceId": physical_resource_id or context.log_stream_name,
        "StackId": event["StackId"],
        "RequestId": event["RequestId"],
        "LogicalResourceId": event["LogicalResourceId"],
        "NoEcho": False,
        "Data": data or {},
    }

    body_bytes = json.dumps(response_body).encode("utf-8")
    req = urllib.request.Request(response_url, data=body_bytes, method="PUT")
    req.add_header("content-type", "")
    req.add_header("content-length", str(len(body_bytes)))

    try:
        with urllib.request.urlopen(req) as resp:
            logger.info("sent CloudFormation response: %s", resp.status)
    except urllib.error.URLError:
        # If we can't signal CFN, logging is the only fallback.
        logger.exception("failed to send CloudFormation response")


def _image_exists(*, ecr, repository_name: str, image_tag: str) -> tuple[bool, str | None]:
    try:
        resp = ecr.describe_images(
            repositoryName=repository_name, imageIds=[{"imageTag": image_tag}]
        )
        details = resp.get("imageDetails") or []
        if not details:
            return False, None
        return True, details[0].get("imageDigest")
    except ClientError as err:
        code = (err.response.get("Error") or {}).get("Code")
        if code in ("RepositoryNotFoundException", "ImageNotFoundException"):
            return False, None
        raise


def _wait_for_image(
    *,
    ecr,
    repository_name: str,
    image_tag: str,
    timeout_seconds: int,
    poll_seconds: int,
    context,
) -> str | None:
    deadline = time.time() + timeout_seconds

    while True:
        exists, digest = _image_exists(
            ecr=ecr, repository_name=repository_name, image_tag=image_tag
        )
        if exists:
            return digest

        remaining = min(deadline - time.time(), context.get_remaining_time_in_millis() / 1000 - 5)
        if remaining <= 0:
            raise TimeoutError(
                f"timed out waiting for image {repository_name}:{image_tag} to exist in ECR"
            )
        time.sleep(min(poll_seconds, remaining))


def handler(event, context):
    logger.info("event: %s", json.dumps(event))

    request_type = event.get("RequestType")
    physical_id = event.get("PhysicalResourceId")

    if request_type == "Delete":
        _send_cfn_response(
            event=event,
            context=context,
            status="SUCCESS",
            physical_resource_id=physical_id,
        )
        return

    props = event.get("ResourceProperties") or {}
    repository_name = str(props.get("RepositoryName") or "").strip()
    image_tag = str(props.get("ImageTag") or "").strip()

    if not repository_name:
        _send_cfn_response(
            event=event,
            context=context,
            status="FAILED",
            reason="RepositoryName is required",
        )
        return

    if not image_tag:
        _send_cfn_response(
            event=event,
            context=context,
            status="FAILED",
            reason="ImageTag is required",
        )
        return

    timeout_seconds = int(props.get("TimeoutSeconds") or 840)
    poll_seconds = int(props.get("PollSeconds") or 5)

    ecr = boto3.client("ecr")
    try:
        digest = _wait_for_image(
            ecr=ecr,
            repository_name=repository_name,
            image_tag=image_tag,
            timeout_seconds=timeout_seconds,
            poll_seconds=poll_seconds,
            context=context,
        )
    except TimeoutError as err:
        _send_cfn_response(
            event=event,
            context=context,
            status="FAILED",
            reason=str(err),
            physical_resource_id=f"{repository_name}:{image_tag}",
        )
        return

    _send_cfn_response(
        event=event,
        context=context,
        status="SUCCESS",
        data={
            "RepositoryName": repository_name,
            "ImageTag": image_tag,
            "ImageDigest": digest or "",
        },
        physical_resource_id=f"{repository_name}:{image_tag}",
    )

