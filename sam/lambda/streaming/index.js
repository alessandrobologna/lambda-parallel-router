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

exports.handler = awslambda.streamifyResponse(async (event, responseStream) => {
  if (typeof responseStream?.setContentType === "function") {
    responseStream.setContentType("application/x-ndjson");
  }

  try {
    const batch = Array.isArray(event?.batch) ? event.batch : [];

    // Write records in completion order.
    await Promise.all(
      batch.map(async (item) => {
        const id = typeof item?.id === "string" ? item.id : "";

        const sleepMs = item?.query?.sleep_ms ?? item?.headers?.["x-sleep-ms"] ?? 0;
        await sleep(sleepMs);

        const bodyBuf = decodeBody(item);
        const out = {
          ok: true,
          id,
          method: item?.method ?? "",
          path: item?.path ?? "",
          route: item?.route ?? "",
          query: item?.query ?? {},
          bodyUtf8: bodyBuf.toString("utf8"),
          sleptMs: Number(sleepMs) || 0,
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

