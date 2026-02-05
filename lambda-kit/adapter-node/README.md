# smug adapter (Node.js)

`batchAdapter(handler)` converts a single-request handler into a batch handler compatible with Simple Multiplexer Gateway.

## Usage

```js
const { batchAdapter } = require("smug-lambda-adapter");

async function handler(event) {
  // `event` is an API Gateway v2 (HTTP API) request event.
  // Correlation id is available as `event.requestContext.requestId`.
  return { statusCode: 200, headers: { "content-type": "text/plain" }, body: "ok" };
}

exports.handler = batchAdapter(handler);
```

## Response streaming (NDJSON)

If your gateway operation uses `invokeMode: response_stream`, export a streaming handler:

```js
const { batchAdapterStream } = require("smug-lambda-adapter");

async function handler(event) {
  return { statusCode: 200, headers: { "content-type": "text/plain" }, body: "ok" };
}

exports.handler = batchAdapterStream(handler);
```

## Interleaved streaming (NDJSON framing)

Use the `interleaved` option to emit `head`/`chunk`/`end` records. This allows
chunk-level streaming per request while still batching multiple requests in a
single Lambda response stream.

```js
const { batchAdapterStream } = require("smug-lambda-adapter");

async function handler(event) {
  async function* body() {
    yield "data: hello\n\n";
    yield "data: world\n\n";
  }

  return {
    statusCode: 200,
    headers: { "content-type": "text/event-stream" },
    body: body(),
  };
}

exports.handler = batchAdapterStream(handler, { interleaved: true });
```

Notes:
- `body` may be a string, Buffer, `AsyncIterable`, or Node `Readable`.
- The gateway should demux the NDJSON records and forward the bytes to clients.
