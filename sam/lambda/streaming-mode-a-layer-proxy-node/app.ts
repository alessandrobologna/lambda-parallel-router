const MAX_DELAY_MS = 10_000;

type ApiGatewayV2Event = {
  body?: string;
  isBase64Encoded?: boolean;
  rawPath?: string;
  routeKey?: string;
  requestContext?: { requestId?: string; http?: { method?: string }; routeKey?: string };
  queryStringParameters?: Record<string, string>;
  pathParameters?: Record<string, string>;
};

type ApiGatewayV2Response = {
  statusCode: number;
  headers?: Record<string, string>;
  body?: string;
  isBase64Encoded?: boolean;
};

function sleep(ms: number): Promise<void> {
  const n = Number(ms);
  if (!Number.isFinite(n) || n <= 0) return Promise.resolve();
  return new Promise((resolve) => setTimeout(resolve, Math.floor(n)));
}

function parseDelayMs(event: ApiGatewayV2Event): number {
  const query = event?.queryStringParameters ?? {};
  const raw = query["max-delay"] ?? 0;
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return 0;
  return Math.min(Math.floor(n), MAX_DELAY_MS);
}

function decodeBodyUtf8(event: ApiGatewayV2Event): string {
  const body = typeof event?.body === "string" ? event.body : "";
  if (!body) return "";
  if (event?.isBase64Encoded) {
    try {
      return Buffer.from(body, "base64").toString("utf8");
    } catch {
      return "";
    }
  }
  return body;
}

export async function handler(event: ApiGatewayV2Event): Promise<ApiGatewayV2Response> {
  const requestId = event?.requestContext?.requestId ?? "";
  const method = event?.requestContext?.http?.method ?? "";
  const path = event?.rawPath ?? "";
  const routeKey = event?.routeKey ?? event?.requestContext?.routeKey ?? "";
  const greeting = event?.pathParameters?.greeting ?? "";

  const maxDelayMs = parseDelayMs(event);
  const delayMs = maxDelayMs ? Math.floor(Math.random() * (maxDelayMs + 1)) : 0;
  await sleep(delayMs);

  const out = {
    ok: true,
    id: requestId,
    greeting,
    method,
    path,
    routeKey,
    query: event?.queryStringParameters ?? {},
    pathParameters: event?.pathParameters ?? {},
    maxDelayMs,
    delayMs,
    bodyUtf8: decodeBodyUtf8(event),
  };

  return {
    statusCode: 200,
    headers: { "content-type": "application/json" },
    body: JSON.stringify(out),
    isBase64Encoded: false,
  };
}
