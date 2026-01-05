"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.batchAdapter = batchAdapter;
exports.batchAdapterStream = batchAdapterStream;
function normalizeConcurrency(value) {
    const n = Number(value);
    if (!Number.isFinite(n) || n < 1)
        return 1;
    return Math.floor(n);
}
async function mapConcurrent(items, concurrency, fn) {
    const out = new Array(items.length);
    let next = 0;
    async function worker() {
        // eslint-disable-next-line no-constant-condition
        while (true) {
            const i = next++;
            if (i >= items.length)
                return;
            // eslint-disable-next-line no-await-in-loop
            out[i] = await fn(items[i], i);
        }
    }
    const workers = Array.from({ length: Math.min(concurrency, items.length) }, () => worker());
    await Promise.all(workers);
    return out;
}
async function forEachConcurrent(items, concurrency, fn) {
    let next = 0;
    async function worker() {
        // eslint-disable-next-line no-constant-condition
        while (true) {
            const i = next++;
            if (i >= items.length)
                return;
            // eslint-disable-next-line no-await-in-loop
            await fn(items[i], i);
        }
    }
    const workers = Array.from({ length: Math.min(concurrency, items.length) }, () => worker());
    await Promise.all(workers);
}
function getRequestId(item) {
    if (typeof item?.requestContext?.requestId === "string")
        return item.requestContext.requestId;
    if (typeof item?.id === "string")
        return item.id;
    return "";
}
function normalizeHeaders(headers) {
    if (!headers || typeof headers !== "object")
        return {};
    const out = {};
    for (const [k, v] of Object.entries(headers)) {
        if (v == null)
            continue;
        out[String(k)] = String(v);
    }
    return out;
}
function normalizeResponseBody(body, isBase64Encoded) {
    if (body == null)
        return { body: "", isBase64Encoded: Boolean(isBase64Encoded) };
    if (Buffer.isBuffer(body)) {
        return { body: body.toString("base64"), isBase64Encoded: true };
    }
    const bodyStr = typeof body === "string" ? body : String(body);
    return { body: bodyStr, isBase64Encoded: Boolean(isBase64Encoded) };
}
function normalizeChunk(chunk, isBase64Encoded) {
    if (chunk == null)
        return { body: "", isBase64Encoded: Boolean(isBase64Encoded) };
    if (Buffer.isBuffer(chunk)) {
        return { body: chunk.toString("base64"), isBase64Encoded: true };
    }
    if (chunk instanceof Uint8Array) {
        return { body: Buffer.from(chunk).toString("base64"), isBase64Encoded: true };
    }
    const bodyStr = typeof chunk === "string" ? chunk : String(chunk);
    return { body: bodyStr, isBase64Encoded: Boolean(isBase64Encoded) };
}
function isAsyncIterable(value) {
    return value && typeof value[Symbol.asyncIterator] === "function";
}
function isIterable(value) {
    return value && typeof value[Symbol.iterator] === "function";
}
async function* toAsyncIterable(body) {
    if (body == null)
        return;
    if (typeof body === "string" || Buffer.isBuffer(body) || body instanceof Uint8Array) {
        yield body;
        return;
    }
    if (isAsyncIterable(body)) {
        for await (const chunk of body) {
            yield chunk;
        }
        return;
    }
    if (isIterable(body)) {
        for (const chunk of body) {
            yield chunk;
        }
        return;
    }
    yield String(body);
}
async function writeNdjsonLine(responseStream, record) {
    const line = `${JSON.stringify(record)}\n`;
    const ok = responseStream?.write?.(line);
    if (ok === false && typeof responseStream?.once === "function") {
        await new Promise((resolve) => responseStream.once("drain", resolve));
    }
}
function batchAdapter(userHandler, options = {}) {
    if (typeof userHandler !== "function") {
        throw new TypeError("batchAdapter(userHandler): userHandler must be a function");
    }
    const concurrency = normalizeConcurrency(options.concurrency ?? 16);
    return async function handler(event, context) {
        const batch = Array.isArray(event?.batch) ? event.batch : [];
        const responses = await mapConcurrent(batch, concurrency, async (item) => {
            const id = getRequestId(item);
            try {
                const userResp = await userHandler(item, context);
                const statusCode = Number(userResp?.statusCode ?? 200);
                const headers = normalizeHeaders(userResp?.headers);
                const { body, isBase64Encoded } = normalizeResponseBody(userResp?.body, userResp?.isBase64Encoded);
                return { id, statusCode, headers, body, isBase64Encoded };
            }
            catch (err) {
                return {
                    id,
                    statusCode: 500,
                    headers: { "content-type": "text/plain" },
                    body: "internal error",
                    isBase64Encoded: false,
                };
            }
        });
        return { v: 1, responses };
    };
}
function batchAdapterStream(userHandler, options = {}) {
    if (typeof userHandler !== "function") {
        throw new TypeError("batchAdapterStream(userHandler): userHandler must be a function");
    }
    const concurrency = normalizeConcurrency(options.concurrency ?? 16);
    const interleaved = Boolean(options.interleaved);
    const streamifyResponse = options.streamifyResponse ?? globalThis?.awslambda?.streamifyResponse;
    if (typeof streamifyResponse !== "function") {
        throw new Error("Response streaming is not available (missing awslambda.streamifyResponse); use batchAdapter() instead.");
    }
    return streamifyResponse(async (event, responseStream, context) => {
        try {
            if (typeof responseStream?.setContentType === "function") {
                responseStream.setContentType("application/x-ndjson");
            }
            const batch = Array.isArray(event?.batch) ? event.batch : [];
            if (!interleaved) {
                await forEachConcurrent(batch, concurrency, async (item) => {
                    const id = getRequestId(item);
                    let record;
                    try {
                        const userResp = await userHandler(item, context);
                        const statusCode = Number(userResp?.statusCode ?? 200);
                        const headers = normalizeHeaders(userResp?.headers);
                        const { body, isBase64Encoded } = normalizeResponseBody(userResp?.body, userResp?.isBase64Encoded);
                        record = { v: 1, id, statusCode, headers, body, isBase64Encoded };
                    }
                    catch (err) {
                        record = {
                            v: 1,
                            id,
                            statusCode: 500,
                            headers: { "content-type": "text/plain" },
                            body: "internal error",
                            isBase64Encoded: false,
                        };
                    }
                    await writeNdjsonLine(responseStream, record);
                });
                return;
            }
            await forEachConcurrent(batch, concurrency, async (item) => {
                const id = getRequestId(item);
                let userResp;
                try {
                    userResp = await userHandler(item, context);
                }
                catch (err) {
                    await writeNdjsonLine(responseStream, {
                        v: 1,
                        id,
                        type: "error",
                        statusCode: 500,
                        message: "internal error",
                    });
                    return;
                }
                const statusCode = Number(userResp?.statusCode ?? 200);
                const headers = normalizeHeaders(userResp?.headers);
                await writeNdjsonLine(responseStream, {
                    v: 1,
                    id,
                    type: "head",
                    statusCode,
                    headers,
                });
                const bodySource = userResp?.body;
                try {
                    for await (const chunk of toAsyncIterable(bodySource)) {
                        const normalized = normalizeChunk(chunk, userResp?.isBase64Encoded);
                        await writeNdjsonLine(responseStream, {
                            v: 1,
                            id,
                            type: "chunk",
                            body: normalized.body,
                            isBase64Encoded: normalized.isBase64Encoded,
                        });
                    }
                    await writeNdjsonLine(responseStream, { v: 1, id, type: "end" });
                }
                catch (err) {
                    await writeNdjsonLine(responseStream, {
                        v: 1,
                        id,
                        type: "error",
                        statusCode: 500,
                        message: "stream error",
                    });
                }
            });
        }
        finally {
            responseStream.end();
        }
    });
}
