"use strict";

function decodeBody(item) {
  const body = typeof item?.body === "string" ? item.body : "";
  const isB64 = Boolean(item?.isBase64Encoded);
  return Buffer.from(body, isB64 ? "base64" : "utf8");
}

exports.handler = async function handler(event) {
  const batch = Array.isArray(event?.batch) ? event.batch : [];

  const responses = batch.map((item) => {
    const id = typeof item?.id === "string" ? item.id : "";
    const bodyBuf = decodeBody(item);

    const out = {
      ok: true,
      id,
      method: item?.method ?? "",
      path: item?.path ?? "",
      route: item?.route ?? "",
      query: item?.query ?? {},
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

