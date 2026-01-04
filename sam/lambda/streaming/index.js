"use strict";

function sleep(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n) || n <= 0) return Promise.resolve();
  return new Promise((r) => setTimeout(r, Math.floor(n)));
}

function decodeBody(item) {
  const body = typeof item?.body === "string" ? item.body : "";
  const isB64 = Boolean(item?.isBase64Encoded);
  return Buffer.from(body, isB64 ? "base64" : "utf8");
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

exports.handler = awslambda.streamifyResponse(async (event, responseStream) => {
  if (typeof responseStream?.setContentType === "function") {
    responseStream.setContentType("application/x-ndjson");
  }

  try {
    const batch = Array.isArray(event?.batch) ? event.batch : [];

    // Write records in completion order.
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
          method: item?.requestContext?.http?.method ?? item?.httpMethod ?? item?.method ?? "",
          path: item?.rawPath ?? item?.path ?? "",
          routeKey: item?.routeKey ?? item?.requestContext?.routeKey ?? "",
          query: item?.queryStringParameters ?? item?.query ?? {},
          bodyUtf8: bodyBuf.toString("utf8"),
          maxDelayMs,
          delayMs,
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
});
