"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");

const { batchAdapter, batchAdapterStream } = require("../index");

test("batchAdapter returns v1 responses array", async () => {
  const handler = batchAdapter(async (evt) => {
    return {
      statusCode: 200,
      headers: { "x-id": evt.requestContext.requestId },
      body: "ok",
      isBase64Encoded: false,
    };
  });

  const out = await handler({
    v: 1,
    batch: [{ requestContext: { requestId: "a" } }, { requestContext: { requestId: "b" } }],
  });
  assert.equal(out.v, 1);
  assert.equal(out.responses.length, 2);
  assert.deepEqual(out.responses[0], {
    id: "a",
    statusCode: 200,
    headers: { "x-id": "a" },
    body: "ok",
    isBase64Encoded: false,
  });
});

test("passes request event items to user handler", async () => {
  const handler = batchAdapter(async (evt) => {
    assert.equal(evt.requestContext.requestId, "a");
    assert.equal(evt.body, Buffer.from("hello", "utf8").toString("base64"));
    assert.equal(evt.isBase64Encoded, true);
    return { statusCode: 200, headers: {}, body: "ok", isBase64Encoded: false };
  });

  const out = await handler({
    v: 1,
    batch: [
      {
        requestContext: { requestId: "a" },
        body: Buffer.from("hello", "utf8").toString("base64"),
        isBase64Encoded: true,
      },
    ],
  });

  assert.equal(out.responses[0].statusCode, 200);
});

test("encodes Buffer response bodies as base64", async () => {
  const handler = batchAdapter(async () => {
    return { statusCode: 200, headers: {}, body: Buffer.from("bin", "utf8") };
  });

  const out = await handler({ v: 1, batch: [{ requestContext: { requestId: "a" } }] });
  assert.equal(out.responses[0].isBase64Encoded, true);
  assert.equal(Buffer.from(out.responses[0].body, "base64").toString("utf8"), "bin");
});

test("limits handler concurrency", async () => {
  let inFlight = 0;
  let maxInFlight = 0;

  const handler = batchAdapter(
    async () => {
      inFlight += 1;
      maxInFlight = Math.max(maxInFlight, inFlight);
      await new Promise((r) => setTimeout(r, 20));
      inFlight -= 1;
      return { statusCode: 200, headers: {}, body: "ok", isBase64Encoded: false };
    },
    { concurrency: 2 },
  );

  await handler({
    v: 1,
    batch: [
      { requestContext: { requestId: "a" } },
      { requestContext: { requestId: "b" } },
      { requestContext: { requestId: "c" } },
      { requestContext: { requestId: "d" } },
      { requestContext: { requestId: "e" } },
    ],
  });

  assert.ok(maxInFlight <= 2);
});

test("returns 500 for thrown handler errors", async () => {
  const handler = batchAdapter(async (evt) => {
    if (evt.requestContext.requestId === "b") throw new Error("boom");
    return { statusCode: 200, headers: {}, body: "ok", isBase64Encoded: false };
  });

  const out = await handler({
    v: 1,
    batch: [{ requestContext: { requestId: "a" } }, { requestContext: { requestId: "b" } }],
  });
  const b = out.responses.find((r) => r.id === "b");
  assert.equal(b.statusCode, 500);
});

test("batchAdapterStream writes NDJSON response records", async () => {
  const writes = [];
  let ended = false;
  let contentType = null;

  const responseStream = {
    setContentType: (ct) => {
      contentType = ct;
    },
    write: (s) => {
      writes.push(String(s));
    },
    end: () => {
      ended = true;
    },
  };

  const streamifyResponse = (fn) => fn;
  const handler = batchAdapterStream(
    async (evt) => {
      return {
        statusCode: 200,
        headers: {},
        body: evt.requestContext.requestId,
        isBase64Encoded: false,
      };
    },
    { streamifyResponse, concurrency: 2 },
  );

  await handler(
    { v: 1, batch: [{ requestContext: { requestId: "a" } }, { requestContext: { requestId: "b" } }] },
    responseStream,
  );

  assert.equal(contentType, "application/x-ndjson");
  assert.equal(ended, true);
  const lines = writes.join("").trim().split("\n");
  assert.equal(lines.length, 2);
  const records = lines.map((l) => JSON.parse(l));

  for (const r of records) {
    assert.equal(r.v, 1);
    assert.equal(r.statusCode, 200);
    assert.equal(r.isBase64Encoded, false);
  }
});
