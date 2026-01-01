# lpr adapter (Node.js)

`batchAdapter(handler)` converts a single-request handler into a batch handler compatible with `lambda-parallel-router`.

## Usage

```js
const { batchAdapter } = require("./index");

async function handler(req) {
  // `req.body` is a Buffer (decoded from base64 when `isBase64Encoded` is true).
  return { statusCode: 200, headers: { "content-type": "text/plain" }, body: "ok" };
}

exports.handler = batchAdapter(handler);
```
