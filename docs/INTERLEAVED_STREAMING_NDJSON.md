# Interleaved streaming (NDJSON framing) - design note

This document describes a possible **interleaved streaming** protocol for the router.
It keeps **NDJSON as the internal framing** but allows the Lambda to decide the
**client-facing response format** (e.g., SSE, JSON, text, binary).

The key idea is: a single Lambda invocation returns a stream of NDJSON records,
each tagged with a request `id`, and the router demultiplexes those records into
per-request HTTP response streams.

---

## Goals

- Support **streaming responses** for individual requests while still batching.
- Allow **interleaving** chunks from multiple requests in a single Lambda stream.
- Let the Lambda define **client-facing protocol** (SSE, JSON, plain text, etc.).
- Keep parsing simple and robust (line-delimited JSON records).

Non-goals:
- Byte-level multiplexing of arbitrary HTTP responses without framing.
- Complex per-request flow-control beyond what HTTP already provides.

---

## Record types

Each NDJSON line is a JSON object with:

```
{
  "v": 1,
  "id": "request-id",
  "type": "head" | "chunk" | "end" | "error",
  ...fields...
}
```

### 1) `head`
Establishes status + headers for a request stream.

```
{
  "v": 1,
  "id": "r1",
  "type": "head",
  "statusCode": 200,
  "headers": { "content-type": "text/event-stream" }
}
```

Rules:
- Must appear **once per request** before the first `chunk`.
- If absent, router MAY synthesize a default `200` and empty headers.

### 2) `chunk`
Contains a chunk of body bytes.

```
{
  "v": 1,
  "id": "r1",
  "type": "chunk",
  "body": "data: hello\n\n",
  "isBase64Encoded": false
}
```

Rules:
- `body` is a string; `isBase64Encoded` indicates decoding before send.
- Router writes chunk to the corresponding client response body.

### 3) `end`
Marks the end of the response stream for `id`.

```
{ "v": 1, "id": "r1", "type": "end" }
```

Rules:
- Router finalizes the response and closes the stream.
- Missing `end` implies error/timeout handling by router policy.

### 4) `error`
Indicates a request-specific failure.

```
{
  "v": 1,
  "id": "r1",
  "type": "error",
  "statusCode": 502,
  "message": "upstream failed"
}
```

Rules:
- Router sends an error response (if not already started) and closes the stream.

---

## Interleaving example

Two requests (`r1`, `r2`) interleaved in one Lambda response stream:

```
{"v":1,"id":"r1","type":"head","statusCode":200,"headers":{"content-type":"text/event-stream"}}
{"v":1,"id":"r1","type":"chunk","body":"data: hello\n\n","isBase64Encoded":false}
{"v":1,"id":"r2","type":"head","statusCode":200,"headers":{"content-type":"application/json"}}
{"v":1,"id":"r2","type":"chunk","body":"{\"partial\":1}\\n","isBase64Encoded":false}
{"v":1,"id":"r1","type":"chunk","body":"data: world\n\n","isBase64Encoded":false}
{"v":1,"id":"r2","type":"end"}
{"v":1,"id":"r1","type":"end"}
```

The router demuxes each record to the correct client stream.

---

## SSE as client-facing protocol

The Lambda can generate SSE **payload bytes** itself and just stream them via
`chunk` records:

```
{"v":1,"id":"r1","type":"head","statusCode":200,"headers":{"content-type":"text/event-stream"}}
{"v":1,"id":"r1","type":"chunk","body":"data: token1\\n\\n","isBase64Encoded":false}
{"v":1,"id":"r1","type":"chunk","body":"data: token2\\n\\n","isBase64Encoded":false}
{"v":1,"id":"r1","type":"end"}
```

This keeps NDJSON internal while still providing **native SSE** to clients.

---

## Router behavior (high level)

- Parse NDJSON line-by-line from the Lambda response stream.
- Validate each record; unknown `id` ⇒ error policy (e.g., drop + log).
- For each `id`, maintain per-request state:
  - `head` received?
  - response sender open?
  - finished?
- On `head`, start the HTTP response stream with given status + headers.
- On `chunk`, write bytes to the response body.
- On `end`, close the stream.
- On `error`, send error response (if not already started) and close.

---

## Backpressure & buffering

- Router should apply **bounded buffering** per request to avoid memory blowup.
- If a client is slow, router can:
  - apply backpressure to the Lambda stream (best-effort), or
  - fail that request with `error` and continue others.

---

## Compatibility with buffered mode

Buffered responses remain unchanged:

```
{ "id": "r1", "statusCode": 200, "headers": {...}, "body": "...", "isBase64Encoded": false }
```

Interleaved streaming is **opt-in** via route config (e.g., `invokeMode: response_stream`).

---

## Open questions

- Do we require `head` explicitly, or allow implicit defaults?
- Should `error` close all requests or only the specific `id`?
- Do we support trailers or `content-length` for streams? (likely no)
