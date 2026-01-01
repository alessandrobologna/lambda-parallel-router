//! `lpr-router` is the Rust implementation of **lambda-parallel-router**.
//!
//! The router is a long-running HTTP service that micro-batches incoming requests per route for a
//! configurable amount of time and invokes AWS Lambda with a single batched payload. Responses are
//! then demultiplexed back to the original callers.
//!
//! Core modules:
//! - [`config`]: YAML router config
//! - [`spec`]: OpenAPI-ish routing spec + matcher
//! - [`batching`]: microbatch queues + request/response demux
//! - [`lambda`]: AWS Lambda invocation (buffered or response stream)
//! - [`server`]: axum server wiring

pub mod batching;
pub mod config;
pub mod lambda;
pub mod server;
pub mod spec;
