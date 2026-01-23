#!/usr/bin/env bash
set -euo pipefail

export LPR_PROXY_ADDR="${LPR_PROXY_ADDR:-127.0.0.1:9009}"

# The Lambda managed runtimes decide how many concurrent Runtime API workers to run by reading
# `AWS_LAMBDA_MAX_CONCURRENCY`. Mode A relies on those workers to pull multiple "virtual"
# invocations from the local proxy concurrently.
#
# NOTE: As of python:3.14.v32, enabling this for Python while `_LAMBDA_TELEMETRY_LOG_FD` is set can
# cause the runtime to crash during init (OSError: [Errno 9] Bad file descriptor in awslambdaric
# log sink). See workaround below.
#
# Keep the wrapper safe-by-default and only set it when explicitly requested via `LPR_MAX_CONCURRENCY`.
if [[ -z "${AWS_LAMBDA_MAX_CONCURRENCY:-}" && -n "${LPR_MAX_CONCURRENCY:-}" ]]; then
  export AWS_LAMBDA_MAX_CONCURRENCY="${LPR_MAX_CONCURRENCY}"
fi

# Python 3.14 uses `multiprocessing` to implement elevator mode. On Linux, the default start method
# is `forkserver`, which does not preserve the parent process's file descriptor numbers in workers.
# When `_LAMBDA_TELEMETRY_LOG_FD` is set, awslambdaric tries to `os.fdopen()` that numeric fd inside
# each worker, which can crash with "Bad file descriptor".
#
# Workaround: if we enable elevator mode for Python, force awslambdaric to fall back to stdout/stderr
# logging by unsetting `_LAMBDA_TELEMETRY_LOG_FD`. (On real Lambda Managed Instances, the platform
# uses `_LAMBDA_TELEMETRY_LOG_FD_PROVIDER_SOCKET` instead, and `_LAMBDA_TELEMETRY_LOG_FD` is unset.)
if [[ "${AWS_EXECUTION_ENV:-}" == *python* && -n "${AWS_LAMBDA_MAX_CONCURRENCY:-}" && -n "${_LAMBDA_TELEMETRY_LOG_FD:-}" ]]; then
  unset _LAMBDA_TELEMETRY_LOG_FD
fi

if [[ -z "${LPR_UPSTREAM_RUNTIME_API:-}" ]]; then
  export LPR_UPSTREAM_RUNTIME_API="${AWS_LAMBDA_RUNTIME_API}"
fi

export AWS_LAMBDA_RUNTIME_API="${LPR_PROXY_ADDR}"

exec "$@"
