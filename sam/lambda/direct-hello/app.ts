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

function parseItemKey(event: FunctionUrlEvent): string {
  const rawPath = typeof event?.rawPath === "string" ? event.rawPath : "";
  const parts = rawPath.split("/").filter((p) => p.length > 0);
  const last = parts.length > 0 ? parts[parts.length - 1] : "";
  return last || "hello";
}

export async function handler(event: FunctionUrlEvent): Promise<FunctionUrlResponse> {
  const requestId = event?.requestContext?.requestId ?? "";
  const method = event?.requestContext?.http?.method ?? "";
  const path = event?.rawPath ?? "";
  const itemKey = parseItemKey(event);

  const maxDelayMs = parseDelayMs(event);
  const delayMs = maxDelayMs ? Math.floor(Math.random() * (maxDelayMs + 1)) : 0;
  await sleep(delayMs);

  const bodyBuf = decodeBody(event);
  const payload = await getItemPayload(itemKey);

  const out = {
    ok: true,
    requestId,
    method,
    path,
    query: event?.queryStringParameters ?? {},
    maxDelayMs,
    delayMs,
    itemKey,
    itemFound: payload != null,
    payload,
    bodyUtf8: bodyBuf.toString("utf8"),
  };

  return {
    statusCode: 200,
    headers: { "content-type": "application/json" },
    body: JSON.stringify(out),
    isBase64Encoded: false,
  };
}
