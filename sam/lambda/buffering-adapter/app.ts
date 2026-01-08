const { batchAdapter } = require("../../../lambda-kit/adapter-node/index.js");

const MAX_DELAY_MS = 10_000;

type ApiGatewayV2Event = {
  requestContext?: { requestId?: string; http?: { method?: string }; routeKey?: string };
  rawPath?: string;
  routeKey?: string;
  queryStringParameters?: Record<string, string>;
  pathParameters?: Record<string, string>;
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

function decodeBody(event: ApiGatewayV2Event): string {
  const body = typeof event?.body === "string" ? event.body : "";
  if (!body) return "";
  if (event?.isBase64Encoded) {
    return Buffer.from(body, "base64").toString("utf8");
  }
  return body;
}

export const handler = batchAdapter(async (event: ApiGatewayV2Event) => {
  const maxDelayMs = parseDelayMs(event);
  const delayMs = maxDelayMs ? Math.floor(Math.random() * (maxDelayMs + 1)) : 0;
  await sleep(delayMs);

  const out = {
    ok: true,
    id: event?.requestContext?.requestId ?? "",
    method: event?.requestContext?.http?.method ?? "",
    greeting: event?.pathParameters?.greeting ?? "",
    path: event?.rawPath ?? "",
    routeKey: event?.routeKey ?? event?.requestContext?.routeKey ?? "",
    query: event?.queryStringParameters ?? {},
    pathParameters: event?.pathParameters ?? {},
    maxDelayMs,
    delayMs,
    bodyUtf8: decodeBody(event),
  };

  return {
    statusCode: 200,
    headers: { "content-type": "application/json" },
    body: JSON.stringify(out),
    isBase64Encoded: false,
  };
});
