"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");

const { batchAdapter, batchAdapterStream } = require("../index");

test("batchAdapter returns v1 responses array", async () => {
  const handler = batchAdapter(async (req) => {
    return { statusCode: 200, headers: { "x-id": req.id }, body: "ok", isBase64Encoded: false };
  });

  const out = await handler({ v: 1, batch: [{ id: "a" }, { id: "b" }] });
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

test("decodes base64 request bodies to Buffer", async () => {
  const handler = batchAdapter(async (req) => {
    assert.ok(Buffer.isBuffer(req.body));
    assert.equal(req.body.toString("utf8"), "hello");
    return { statusCode: 200, headers: {}, body: "ok", isBase64Encoded: false };
  });

  const out = await handler({
    v: 1,
    batch: [
      {
        id: "a",
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

  const out = await handler({ v: 1, batch: [{ id: "a" }] });
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
    batch: [{ id: "a" }, { id: "b" }, { id: "c" }, { id: "d" }, { id: "e" }],
  });

  assert.ok(maxInFlight <= 2);
});

test("returns 500 for thrown handler errors", async () => {
  const handler = batchAdapter(async (req) => {
    if (req.id === "b") throw new Error("boom");
    return { statusCode: 200, headers: {}, body: "ok", isBase64Encoded: false };
  });

  const out = await handler({ v: 1, batch: [{ id: "a" }, { id: "b" }] });
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
    async (req) => {
      return { statusCode: 200, headers: {}, body: req.id, isBase64Encoded: false };
    },
    { streamifyResponse, concurrency: 2 },
  );

  await handler({ v: 1, batch: [{ id: "a" }, { id: "b" }] }, responseStream);

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
