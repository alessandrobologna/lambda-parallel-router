# lpr adapter (Node.js)

`batchAdapter(handler)` converts a single-request handler into a batch handler compatible with `lambda-parallel-router`.

## Usage

```js
const { batchAdapter } = require("./index");

async function handler(event) {
  // `event` is an API Gateway v2 (HTTP API) request event.
  // Correlation id is available as `event.requestContext.requestId`.
  return { statusCode: 200, headers: { "content-type": "text/plain" }, body: "ok" };
}

exports.handler = batchAdapter(handler);
```

## Response streaming (NDJSON)

If your router operation uses `invoke_mode: response_stream`, export a streaming handler:

```js
const { batchAdapterStream } = require("./index");

async function handler(event) {
  return { statusCode: 200, headers: { "content-type": "text/plain" }, body: "ok" };
}

exports.handler = batchAdapterStream(handler);
```
