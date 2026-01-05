const { batchAdapterStream } = require("../../../lambda-kit/adapter-node/index.js");

declare const awslambda: any;

const MAX_DELAY_MS = 10_000;
const MAX_TOKENS = 200;

type ApiGatewayV2Event = {
  requestContext?: { requestId?: string; http?: { method?: string }; routeKey?: string };
  rawPath?: string;
  routeKey?: string;
  queryStringParameters?: Record<string, string>;
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

function parseTokenCount(event: ApiGatewayV2Event): number {
  const query = event?.queryStringParameters ?? {};
  const raw = query["tokens"] ?? query["count"] ?? 20;
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return 20;
  return Math.min(Math.floor(n), MAX_TOKENS);
}

function parsePrefix(event: ApiGatewayV2Event): string {
  const query = event?.queryStringParameters ?? {};
  const raw = query["prefix"];
  if (raw == null) return "token";
  const value = String(raw).trim();
  return value.length > 0 ? value : "token";
}

export const handler = batchAdapterStream(
  async (event: ApiGatewayV2Event) => {
    const maxDelayMs = parseDelayMs(event);
    const requestId = event?.requestContext?.requestId ?? "unknown";
    const tokenCount = parseTokenCount(event);
    const prefix = parsePrefix(event);

    async function* body() {
      for (let i = 1; i <= tokenCount; i += 1) {
        const delayMs = maxDelayMs ? Math.floor(Math.random() * (maxDelayMs + 1)) : 0;
        await sleep(delayMs);
        yield `data: ${prefix}-${i}\n\n`;
      }

      yield `data: request=${requestId}\n\n`;
    }

    return {
      statusCode: 200,
      headers: {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
      },
      body: body(),
    };
  },
  { streamifyResponse: awslambda.streamifyResponse, interleaved: true },
);
