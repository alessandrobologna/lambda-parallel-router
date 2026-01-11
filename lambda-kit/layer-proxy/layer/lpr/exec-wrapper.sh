#!/usr/bin/env bash
set -euo pipefail

export LPR_PROXY_ADDR="${LPR_PROXY_ADDR:-127.0.0.1:9009}"

if [[ -z "${LPR_UPSTREAM_RUNTIME_API:-}" ]]; then
  export LPR_UPSTREAM_RUNTIME_API="${AWS_LAMBDA_RUNTIME_API}"
fi

export AWS_LAMBDA_RUNTIME_API="${LPR_PROXY_ADDR}"

exec "$@"
