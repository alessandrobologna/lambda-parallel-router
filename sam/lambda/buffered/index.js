"use strict";

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

exports.handler = async function handler(event) {
  const batch = Array.isArray(event?.batch) ? event.batch : [];

  const responses = batch.map((item) => {
    const id = getRequestId(item);
    const bodyBuf = decodeBody(item);

    const out = {
      ok: true,
      id,
      method: item?.requestContext?.http?.method ?? item?.httpMethod ?? item?.method ?? "",
      path: item?.rawPath ?? item?.path ?? "",
      routeKey: item?.routeKey ?? item?.requestContext?.routeKey ?? "",
      query: item?.queryStringParameters ?? item?.query ?? {},
      bodyUtf8: bodyBuf.toString("utf8"),
    };

    return {
      id,
      statusCode: 200,
      headers: { "content-type": "application/json" },
      body: JSON.stringify(out),
      isBase64Encoded: false,
    };
  });

  return { v: 1, responses };
};
