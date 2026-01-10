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

test("batchAdapter forwards response cookies", async () => {
  const handler = batchAdapter(async () => {
    return {
      statusCode: 200,
      headers: {},
      cookies: ["a=b; Path=/; HttpOnly", "c=d; Path=/; Secure"],
      body: "ok",
      isBase64Encoded: false,
    };
  });

  const out = await handler({
    v: 1,
    batch: [{ requestContext: { requestId: "a" } }],
  });

  assert.deepEqual(out.responses[0].cookies, ["a=b; Path=/; HttpOnly", "c=d; Path=/; Secure"]);
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
        cookies: ["session=abc; Path=/; HttpOnly"],
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
    assert.deepEqual(r.cookies, ["session=abc; Path=/; HttpOnly"]);
  }
});

test("batchAdapterStream interleaved emits head/chunk/end records", async () => {
  const writes = [];
  let ended = false;
  let contentType = null;

  const responseStream = {
    setContentType: (ct) => {
      contentType = ct;
    },
    write: (s) => {
      writes.push(String(s));
      return true;
    },
    end: () => {
      ended = true;
    },
  };

  const streamifyResponse = (fn) => fn;
  const handler = batchAdapterStream(
    async (evt) => {
      async function* body() {
        yield `data: ${evt.requestContext.requestId}\n\n`;
        yield Buffer.from("bin", "utf8");
      }

      return {
        statusCode: 200,
        headers: { "content-type": "text/event-stream" },
        cookies: ["session=abc; Path=/; HttpOnly"],
        body: body(),
      };
    },
    { streamifyResponse, concurrency: 2, interleaved: true },
  );

  await handler(
    { v: 1, batch: [{ requestContext: { requestId: "a" } }, { requestContext: { requestId: "b" } }] },
    responseStream,
  );

  assert.equal(contentType, "application/x-ndjson");
  assert.equal(ended, true);

  const lines = writes.join("").trim().split("\n");
  const records = lines.map((l) => JSON.parse(l));
  const heads = records.filter((r) => r.type === "head");
  const chunks = records.filter((r) => r.type === "chunk");
  const ends = records.filter((r) => r.type === "end");

  assert.equal(heads.length, 2);
  assert.equal(ends.length, 2);
  assert.equal(chunks.length, 4);

  for (const id of ["a", "b"]) {
    const firstIdx = records.findIndex((r) => r.id === id);
    assert.equal(records[firstIdx].type, "head");
    assert.deepEqual(records[firstIdx].cookies, ["session=abc; Path=/; HttpOnly"]);
    assert.ok(records.some((r) => r.id === id && r.type === "chunk"));
    assert.ok(records.some((r) => r.id === id && r.type === "end"));
  }

  assert.ok(chunks.some((r) => r.isBase64Encoded === true));
});
