const MAX_DELAY_MS = 10_000;

declare const awslambda: any;

type BatchItem = {
  body?: string;
  isBase64Encoded?: boolean;
  requestContext?: { requestId?: string; http?: { method?: string }; routeKey?: string };
  id?: string;
  rawPath?: string;
  path?: string;
  routeKey?: string;
  queryStringParameters?: Record<string, string>;
  pathParameters?: Record<string, string>;
  query?: Record<string, string>;
  headers?: Record<string, string>;
  httpMethod?: string;
  method?: string;
};

type BatchEvent = { batch?: BatchItem[] };

type ResponseStream = {
  write: (chunk: string | Uint8Array) => void;
  end: () => void;
  setContentType?: (contentType: string) => void;
};

function decodeBody(item: BatchItem): Buffer {
  const body = typeof item?.body === "string" ? item.body : "";
  const isB64 = Boolean(item?.isBase64Encoded);
  return Buffer.from(body, isB64 ? "base64" : "utf8");
}

function sleep(ms: number): Promise<void> {
  const n = Number(ms);
  if (!Number.isFinite(n) || n <= 0) return Promise.resolve();
  return new Promise((resolve) => setTimeout(resolve, Math.floor(n)));
}

function getRequestId(item: BatchItem): string {
  if (typeof item?.requestContext?.requestId === "string") return item.requestContext.requestId;
  if (typeof item?.id === "string") return item.id;
  return "";
}

function parseDelayMs(item: BatchItem): number {
  const query = item?.queryStringParameters ?? item?.query ?? {};
  const raw = query["max-delay"] ?? 0;
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return 0;
  return Math.min(Math.floor(n), MAX_DELAY_MS);
}

export const handler = awslambda.streamifyResponse(
  async (event: BatchEvent, responseStream: ResponseStream) => {
    if (typeof responseStream?.setContentType === "function") {
      responseStream.setContentType("application/x-ndjson");
    }

    try {
      const batch = Array.isArray(event?.batch) ? event.batch : [];

      await Promise.all(
        batch.map(async (item) => {
          const id = getRequestId(item);
          const maxDelayMs = parseDelayMs(item);
          const delayMs = maxDelayMs ? Math.floor(Math.random() * (maxDelayMs + 1)) : 0;
          await sleep(delayMs);

          const bodyBuf = decodeBody(item);
          const out = {
            ok: true,
            id,
            greeting: item?.pathParameters?.greeting ?? "",
            method: item?.requestContext?.http?.method ?? item?.httpMethod ?? item?.method ?? "",
            path: item?.rawPath ?? item?.path ?? "",
            routeKey: item?.routeKey ?? item?.requestContext?.routeKey ?? "",
            query: item?.queryStringParameters ?? item?.query ?? {},
            pathParameters: item?.pathParameters ?? {},
            maxDelayMs,
            delayMs,
            bodyUtf8: bodyBuf.toString("utf8"),
          };

          const record = {
            v: 1,
            id,
            statusCode: 200,
            headers: { "content-type": "application/json" },
            body: JSON.stringify(out),
            isBase64Encoded: false,
          };

          responseStream.write(`${JSON.stringify(record)}\n`);
        }),
      );
    } finally {
      responseStream.end();
    }
  },
);
