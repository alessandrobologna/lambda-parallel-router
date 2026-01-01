"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");

const { batchAdapter } = require("../index");

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

