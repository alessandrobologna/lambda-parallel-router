"use strict";

function normalizeConcurrency(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || n < 1) return 1;
  return Math.floor(n);
}

async function mapConcurrent(items, concurrency, fn) {
  const out = new Array(items.length);
  let next = 0;

  async function worker() {
    // eslint-disable-next-line no-constant-condition
    while (true) {
      const i = next++;
      if (i >= items.length) return;
      // eslint-disable-next-line no-await-in-loop
      out[i] = await fn(items[i], i);
    }
  }

  const workers = Array.from({ length: Math.min(concurrency, items.length) }, () => worker());
  await Promise.all(workers);
  return out;
}

function decodeRequestBody(item) {
  if (item == null) return Buffer.alloc(0);

  const { body, isBase64Encoded } = item;
  if (typeof body === "string") {
    if (isBase64Encoded) return Buffer.from(body, "base64");
    return Buffer.from(body, "utf8");
  }

  if (body == null) return Buffer.alloc(0);
  if (Buffer.isBuffer(body)) return body;
  return Buffer.from(String(body), "utf8");
}

function normalizeHeaders(headers) {
  if (!headers || typeof headers !== "object") return {};
  const out = {};
  for (const [k, v] of Object.entries(headers)) {
    if (v == null) continue;
    out[String(k)] = String(v);
  }
  return out;
}

function normalizeResponseBody(body, isBase64Encoded) {
  if (body == null) return { body: "", isBase64Encoded: Boolean(isBase64Encoded) };

  if (Buffer.isBuffer(body)) {
    return { body: body.toString("base64"), isBase64Encoded: true };
  }

  const bodyStr = typeof body === "string" ? body : String(body);
  return { body: bodyStr, isBase64Encoded: Boolean(isBase64Encoded) };
}

function batchAdapter(userHandler, options = {}) {
  if (typeof userHandler !== "function") {
    throw new TypeError("batchAdapter(userHandler): userHandler must be a function");
  }
  const concurrency = normalizeConcurrency(options.concurrency ?? 16);

  return async function handler(event) {
    const batch = Array.isArray(event?.batch) ? event.batch : [];

    const responses = await mapConcurrent(batch, concurrency, async (item) => {
      const id = typeof item?.id === "string" ? item.id : "";
      const req = {
        ...item,
        id,
        body: decodeRequestBody(item),
        headers: normalizeHeaders(item?.headers),
        query: item?.query && typeof item.query === "object" ? item.query : {},
      };

      try {
        const userResp = await userHandler(req);
        const statusCode = Number(userResp?.statusCode ?? 200);
        const headers = normalizeHeaders(userResp?.headers);
        const { body, isBase64Encoded } = normalizeResponseBody(
          userResp?.body,
          userResp?.isBase64Encoded,
        );

        return { id, statusCode, headers, body, isBase64Encoded };
      } catch (err) {
        return {
          id,
          statusCode: 500,
          headers: { "content-type": "text/plain" },
          body: "internal error",
          isBase64Encoded: false,
        };
      }
    });

    return { v: 1, responses };
  };
}

module.exports = { batchAdapter };
