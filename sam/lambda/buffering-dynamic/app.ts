const MAX_DELAY_MS = 10_000;

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

type BatchResponse = {
  v: number;
  responses: Array<{
    id: string;
    statusCode: number;
    headers: Record<string, string>;
    body: string;
    isBase64Encoded: boolean;
  }>;
};

type DdbDeps = { client: any; GetItemCommand: any };

let ddbDeps: Promise<DdbDeps> | null = null;

function getDdbDeps(): Promise<DdbDeps> {
  if (!ddbDeps) {
    ddbDeps = import("@aws-sdk/client-dynamodb").then((mod) => ({
      client: new mod.DynamoDBClient({}),
      GetItemCommand: mod.GetItemCommand,
    }));
  }
  return ddbDeps;
}

async function getItemPayload(pk: string): Promise<string | null> {
  const tableName = process.env.BENCHMARK_TABLE_NAME;
  if (!tableName) return null;

  const { client, GetItemCommand } = await getDdbDeps();
  const res = await client.send(
    new GetItemCommand({
      TableName: tableName,
      Key: { pk: { S: pk } },
      ProjectionExpression: "#payload",
      ExpressionAttributeNames: { "#payload": "payload" },
    }),
  );

  const payload = res?.Item?.payload?.S;
  return typeof payload === "string" ? payload : null;
}

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

export async function handler(event: BatchEvent): Promise<BatchResponse> {
  const batch = Array.isArray(event?.batch) ? event.batch : [];

  const responses = await Promise.all(
    batch.map(async (item) => {
      const id = getRequestId(item);
      const bodyBuf = decodeBody(item);

      const maxDelayMs = parseDelayMs(item);
      const delayMs = maxDelayMs ? Math.floor(Math.random() * (maxDelayMs + 1)) : 0;
      await sleep(delayMs);

      const itemKey = item?.pathParameters?.id ?? item?.pathParameters?.greeting ?? "";
      const greeting = itemKey;
      const payload = itemKey ? await getItemPayload(itemKey) : null;

      const out = {
        ok: true,
        id,
        greeting,
        method: item?.requestContext?.http?.method ?? item?.httpMethod ?? item?.method ?? "",
        path: item?.rawPath ?? item?.path ?? "",
        routeKey: item?.routeKey ?? item?.requestContext?.routeKey ?? "",
        query: item?.queryStringParameters ?? item?.query ?? {},
        pathParameters: item?.pathParameters ?? {},
        maxDelayMs,
        delayMs,
        itemKey,
        itemFound: payload != null,
        payload,
        bodyUtf8: bodyBuf.toString("utf8"),
      };

      return {
        id,
        statusCode: 200,
        headers: { "content-type": "application/json" },
        body: JSON.stringify(out),
        isBase64Encoded: false,
      };
    }),
  );

  return { v: 1, responses };
}
