import base64
import json
import os
import random
import time
from typing import Any

import boto3

MAX_DELAY_MS = 10_000

_BENCHMARK_TABLE_NAME = os.environ.get("BENCHMARK_TABLE_NAME") or ""
_DDB = boto3.resource("dynamodb")
_BENCHMARK_TABLE = _DDB.Table(_BENCHMARK_TABLE_NAME) if _BENCHMARK_TABLE_NAME else None


def _parse_delay_ms(event: dict[str, Any]) -> int:
    query = event.get("queryStringParameters") or {}
    if not isinstance(query, dict):
        return 0

    raw = query.get("max-delay", 0)
    try:
        n = int(raw)
    except (TypeError, ValueError):
        return 0

    if n <= 0:
        return 0

    return min(n, MAX_DELAY_MS)


def _decode_body_utf8(event: dict[str, Any]) -> str:
    raw_body = event.get("body") or ""
    if not isinstance(raw_body, str) or not raw_body:
        return ""

    if event.get("isBase64Encoded"):
        try:
            return base64.b64decode(raw_body.encode("utf-8")).decode("utf-8", errors="replace")
        except Exception:
            return ""

    return raw_body


def _get_item_payload(pk: str) -> str | None:
    if not pk or _BENCHMARK_TABLE is None:
        return None
    try:
        res = _BENCHMARK_TABLE.get_item(Key={"pk": pk})
    except Exception:
        return None
    item = res.get("Item") or {}
    payload = item.get("payload")
    return payload if isinstance(payload, str) else None


def handler(event: dict[str, Any], context: Any) -> dict[str, Any]:
    request_context = event.get("requestContext") or {}
    if not isinstance(request_context, dict):
        request_context = {}

    http = request_context.get("http") or {}
    if not isinstance(http, dict):
        http = {}

    path_parameters = event.get("pathParameters") or {}
    if not isinstance(path_parameters, dict):
        path_parameters = {}

    query = event.get("queryStringParameters") or {}
    if not isinstance(query, dict):
        query = {}

    request_id = request_context.get("requestId") or ""
    method = http.get("method") or ""
    path = event.get("rawPath") or ""
    route_key = event.get("routeKey") or request_context.get("routeKey") or ""
    item_key = path_parameters.get("id") or path_parameters.get("greeting") or ""
    greeting = item_key

    max_delay_ms = _parse_delay_ms(event)
    delay_ms = random.randint(0, max_delay_ms) if max_delay_ms else 0
    if delay_ms:
        time.sleep(delay_ms / 1000.0)

    payload = _get_item_payload(str(item_key))

    out = {
        "ok": True,
        "id": request_id,
        "greeting": greeting,
        "method": method,
        "path": path,
        "routeKey": route_key,
        "query": query,
        "pathParameters": path_parameters,
        "maxDelayMs": max_delay_ms,
        "delayMs": delay_ms,
        "itemKey": item_key,
        "itemFound": payload is not None,
        "payload": payload,
        "bodyUtf8": _decode_body_utf8(event),
    }

    return {
        "statusCode": 200,
        "headers": {"content-type": "application/json"},
        "body": json.dumps(out),
        "isBase64Encoded": False,
    }
