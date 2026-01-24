# Interleaved Streaming (NDJSON Framing)

Interleaved streaming is an experimental response format. It allows one Lambda invocation to stream multiple request responses by emitting NDJSON records tagged with a request id. The router demultiplexes records into per-request streams.

Use this when a single invocation handles multiple requests and each request needs incremental output.

## Framing

- Each line is a standalone JSON object.
- Lines are separated by a single newline character.
- The content type should be `application/x-ndjson`.

## Record types

Each NDJSON line is a JSON object with these common fields:

- `v`: protocol version (`1`)
- `id`: request id (matches `requestContext.requestId`)
- `type`: `head`, `chunk`, `end`, or `error`

### `head`

Starts a response stream and defines status and headers.

```json
{"v":1,"id":"r-1","type":"head","statusCode":200,"headers":{"content-type":"text/event-stream"},"cookies":["a=b"]}
```

### `chunk`

Carries a body chunk. `body` is a string. Use `isBase64Encoded: true` for binary data.

```json
{"v":1,"id":"r-1","type":"chunk","body":"data: hello\n\n","isBase64Encoded":false}
```

### `end`

Closes the stream.

```json
{"v":1,"id":"r-1","type":"end"}
```

### `error`

Reports a per-request error. The router returns an error response and closes the stream.

```json
{"v":1,"id":"r-1","type":"error","statusCode":502,"message":"upstream failed"}
```

## Router behavior

- Records are processed in arrival order.
- `head` is optional. If it is omitted, the router synthesizes a default `head` (200, empty headers)
  when it receives the first `chunk` or `end` for that id.
- The router maps `cookies` to `Set-Cookie` headers.
- Unknown ids are dropped.

## Compatibility

- Interleaved streaming is optional and should be treated as experimental.
- Buffered and standard NDJSON response records remain supported.
