"use strict";

function decodeBody(item) {
  const body = typeof item?.body === "string" ? item.body : "";
  const isB64 = Boolean(item?.isBase64Encoded);
  return Buffer.from(body, isB64 ? "base64" : "utf8");
}

function sleep(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n) || n <= 0) return Promise.resolve();
  return new Promise((r) => setTimeout(r, Math.floor(n)));
}

function getRequestId(item) {
  if (typeof item?.requestContext?.requestId === "string") return item.requestContext.requestId;
  if (typeof item?.id === "string") return item.id;
  return "";
}

function parseDelayMs(item) {
  const query = item?.queryStringParameters ?? item?.query ?? {};
  const raw = query?.["max-delay"] ?? 0;

  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return 0;
  // Keep the demo from accidentally sleeping for a very long time.
  return Math.min(Math.floor(n), 10_000);
}

exports.handler = async function handler(event) {
  const batch = Array.isArray(event?.batch) ? event.batch : [];

  const responses = await Promise.all(
    batch.map(async (item) => {
      const id = getRequestId(item);
      const bodyBuf = decodeBody(item);

      const maxDelayMs = parseDelayMs(item);
      const delayMs = maxDelayMs ? Math.floor(Math.random() * (maxDelayMs + 1)) : 0;
      await sleep(delayMs);

      const out = {
        ok: true,
        id,
        method: item?.requestContext?.http?.method ?? item?.httpMethod ?? item?.method ?? "",
        path: item?.rawPath ?? item?.path ?? "",
        routeKey: item?.routeKey ?? item?.requestContext?.routeKey ?? "",
        query: item?.queryStringParameters ?? item?.query ?? {},
        maxDelayMs,
        delayMs,
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
};
