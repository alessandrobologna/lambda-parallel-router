type MaybePromise<T> = T | Promise<T>;
export type HandlerResponse = {
    statusCode?: number;
    headers?: Record<string, string | number | boolean>;
    cookies?: string[];
    body?: unknown;
    isBase64Encoded?: boolean;
};
export type BatchAdapterOptions = {
    concurrency?: number;
};
export type BatchAdapterStreamOptions = {
    concurrency?: number;
    streamifyResponse?: (fn: (...args: any[]) => any) => any;
    interleaved?: boolean;
};
export declare function batchAdapter(userHandler: (event: any, context: any) => MaybePromise<HandlerResponse>, options?: BatchAdapterOptions): (event: any, context: any) => Promise<{
    v: number;
    responses: any[];
}>;
export declare function batchAdapterStream(userHandler: (event: any, context: any) => MaybePromise<HandlerResponse>, options?: BatchAdapterStreamOptions): (event: any, responseStream: any, context: any) => Promise<void>;
export {};
