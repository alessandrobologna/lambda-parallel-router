# Architecture

## System boundary

Simple Multiplexer Gateway is a long-running HTTP service. It accepts requests, batches them per route, and invokes Lambda once per batch.

## Data flow

1. The gateway receives an HTTP request and matches a route in the compiled spec.
2. The gateway buffers requests for up to `maxWaitMs` or until `maxBatchSize`.
3. The gateway invokes Lambda with a batch payload.
4. Lambda returns per-request responses in buffered or streaming form.
5. The gateway demultiplexes responses and returns them to clients.

## Sequence diagram

This diagram shows two client requests multiplexed into one Lambda invocation and demultiplexed back into per-request responses. It illustrates NDJSON streaming with early return.

```mermaid
%%{init: {"htmlLabels": true, "sequence": {"diagramMarginX": 130, "diagramMarginY": 30, "bottomMarginAdj": 130}}}%%
sequenceDiagram
  participant C1 as Client 1
  participant C2 as Client 2
  participant R as Gateway
  participant L as Lambda

  C1->>R: HTTP request (r-1)
  C2->>R: HTTP request (r-2)
  Note over R: Buffer per route and key until maxWaitMs or maxBatchSize
  R->>L: Invoke batch (v:1, batch:[r-1,r-2])
  Note over L: Handler processes items (Mode B adapter or Mode C native batch)
  L-->>R: NDJSON record for r-2
  R-->>C2: HTTP response for r-2
  L-->>R: NDJSON record for r-1
  R-->>C1: HTTP response for r-1
```

Buffered mode returns one JSON document with `responses[]`. The gateway responds after the full batch completes.

## Batch contract

The gateway sends a JSON payload with a version marker, metadata, and a batch array. Each batch item follows the API Gateway HTTP API v2 event shape.

```json
{
  "v": 1,
  "meta": {
    "gateway": "simple-multiplexer-gateway",
    "route": "/hello/{id}",
    "receivedAtMs": 1730000000000
  },
  "batch": [
    {
      "routeKey": "GET /hello/{id}",
      "requestContext": {
        "requestId": "r-1",
        "routeKey": "GET /hello/{id}",
        "http": { "method": "GET", "path": "/hello/1" }
      },
      "rawPath": "/hello/1",
      "headers": { "accept": "application/json" },
      "queryStringParameters": { "max-delay": "0" },
      "pathParameters": { "id": "1" },
      "body": "",
      "isBase64Encoded": false
    }
  ]
}
```

The `v` field is the protocol version. Mode A requires `v: 1` to detect SMUG batches. The gateway always includes `v: 1`.

Batch request requirements:

- `v` is required and must be `1`.
- `meta.route` is required and matches the compiled route template.
- Each batch item must include `routeKey`, `requestContext.requestId`, `requestContext.http.method`, and `rawPath`.
- The gateway uses `requestContext.requestId` as the response `id`.

## Response contracts

Buffered and standard NDJSON response records must include `id` and `statusCode`. Records with unknown `id` values are dropped.
Response records may include a `cookies` array. The gateway maps these to `Set-Cookie` headers.

### Buffered response

```json
{
  "v": 1,
  "responses": [
    {
      "id": "r-1",
      "statusCode": 200,
      "headers": { "content-type": "application/json" },
      "body": "{\"ok\":true}",
      "isBase64Encoded": false
    }
  ]
}
```

Buffered responses can return in any order. The gateway waits for a response per buffered request or times out the batch.

### Streaming response (NDJSON)

```json
{"v":1,"id":"r-1","statusCode":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}","isBase64Encoded":false}
```

Each line is a complete response record. Records arrive in completion order.

Streaming response constraints:

- The gateway does not reorder stream records.
- Missing `id` values cause the record to be dropped.

### Interleaved streaming (experimental)

Interleaved streaming uses NDJSON records with `head`, `chunk`, and `end` events. The gateway demultiplexes these into per-request streams.

```json
{"v":1,"id":"r-1","type":"head","statusCode":200,"headers":{"content-type":"text/event-stream"}}
{"v":1,"id":"r-1","type":"chunk","body":"data: hello\n\n","isBase64Encoded":false}
{"v":1,"id":"r-1","type":"end"}
```

This format is experimental and intended for advanced streaming use cases.
See [docs/interleaved-streaming.md](interleaved-streaming.md) for the framing rules.

Interleaved streaming constraints:

- `head` is optional. If it is omitted, the gateway synthesizes a default `head` (200, empty headers)
  when it receives the first `chunk` or `end` for that id.
- The gateway closes the stream on the first `end` for a given id.

## Batching behavior

- Batching is per route and per batch key.
- The batch key includes the target Lambda, method, and route template.
- Additional key dimensions can be set with `x-smug.key` to prevent unsafe co-batching.
- `dynamicWait` adjusts the wait window based on observed request rate.

## Timing and splitting

- The gateway starts the `maxWaitMs` timer when the first request arrives for a batch key.
- When `maxBatchSize` is reached, the gateway invokes Lambda immediately.
- When the payload exceeds `MaxInvokePayloadBytes`, the gateway splits the batch at request boundaries.

## Payload limits

`MaxInvokePayloadBytes` defaults to 6 MiB (6 * 1024 * 1024 bytes). Oversized batches are split when possible. A single request that exceeds the limit fails and returns a 502.

## Non-goals

- Cross-route batching.
- Byte-level multiplexed streaming across requests.
- Full API Gateway feature parity.
- Automatic deduplication or caching.
- WebSocket or bidirectional protocols.
- Automatic request retries.
