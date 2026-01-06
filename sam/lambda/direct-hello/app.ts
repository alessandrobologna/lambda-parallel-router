const MAX_DELAY_MS = 10_000;

type FunctionUrlEvent = {
  body?: string;
  isBase64Encoded?: boolean;
  rawPath?: string;
  requestContext?: { requestId?: string; http?: { method?: string } };
  queryStringParameters?: Record<string, string>;
};

type FunctionUrlResponse = {
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

function decodeBody(event: FunctionUrlEvent): Buffer {
  const body = typeof event?.body === "string" ? event.body : "";
  const isB64 = Boolean(event?.isBase64Encoded);
  return Buffer.from(body, isB64 ? "base64" : "utf8");
}

function parseDelayMs(event: FunctionUrlEvent): number {
  const query = event?.queryStringParameters ?? {};
  const raw = query["max-delay"] ?? 0;
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return 0;
  return Math.min(Math.floor(n), MAX_DELAY_MS);
}

export async function handler(event: FunctionUrlEvent): Promise<FunctionUrlResponse> {
  const requestId = event?.requestContext?.requestId ?? "";
  const method = event?.requestContext?.http?.method ?? "";
  const path = event?.rawPath ?? "";

  const maxDelayMs = parseDelayMs(event);
  const delayMs = maxDelayMs ? Math.floor(Math.random() * (maxDelayMs + 1)) : 0;
  await sleep(delayMs);

  const bodyBuf = decodeBody(event);

  const out = {
    ok: true,
    requestId,
    method,
    path,
    query: event?.queryStringParameters ?? {},
    maxDelayMs,
    delayMs,
    bodyUtf8: bodyBuf.toString("utf8"),
  };

  return {
    statusCode: 200,
    headers: { "content-type": "application/json" },
    body: JSON.stringify(out),
    isBase64Encoded: false,
  };
}

