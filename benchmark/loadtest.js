import http from 'k6/http';
import { check, sleep } from 'k6';
import exec from 'k6/execution';
import { Trend } from 'k6/metrics';

const MODE = __ENV.MODE || 'per_endpoint';
const EXECUTOR = __ENV.EXECUTOR || 'ramping-arrival-rate';
const TARGETS_JSON = __ENV.TARGETS;
const STAGES_JSON = __ENV.STAGES;
const DURATION = __ENV.DURATION || '30s';
const VUS = parseInt(__ENV.VUS || '50', 10);
const ARRIVAL_TIME_UNIT = __ENV.ARRIVAL_TIME_UNIT || '1s';
const ARRIVAL_PREALLOCATED_VUS = parseInt(__ENV.ARRIVAL_PREALLOCATED_VUS || '0', 10);
const ARRIVAL_MAX_VUS = parseInt(__ENV.ARRIVAL_MAX_VUS || '0', 10);
const ARRIVAL_VUS_MULTIPLIER = parseFloat(__ENV.ARRIVAL_VUS_MULTIPLIER || '1');
const ARRIVAL_MAX_VUS_MULTIPLIER = parseFloat(__ENV.ARRIVAL_MAX_VUS_MULTIPLIER || '2');
const MAX_DELAY_MS = parseInt(__ENV.MAX_DELAY_MS || '0', 10);
const KEYSPACE_SIZE = parseInt(__ENV.KEYSPACE_SIZE || '1000', 10);
const BATCH_SIZE_HEADER = (__ENV.BATCH_SIZE_HEADER || 'x-lpr-batch-size').trim();

const batchSizeTrend = new Trend('lpr_batch_size');

if (!TARGETS_JSON) {
  throw new Error('Missing TARGETS JSON. Provide via env TARGETS.');
}

function parseTargets() {
  let parsed;
  try {
    parsed = JSON.parse(TARGETS_JSON);
  } catch (e) {
    throw new Error(`Invalid TARGETS JSON: ${e.message}`);
  }

  if (!Array.isArray(parsed) || parsed.length < 1) {
    throw new Error('Invalid TARGETS. Must be a JSON array with at least 1 item.');
  }

  for (const [i, t] of parsed.entries()) {
    if (!t || typeof t !== 'object') {
      throw new Error(`Invalid TARGETS[${i}]. Must be an object.`);
    }
    if (typeof t.name !== 'string' || t.name.length === 0) {
      throw new Error(`Invalid TARGETS[${i}].name. Must be a non-empty string.`);
    }
    if (typeof t.url !== 'string' || t.url.length === 0) {
      throw new Error(`Invalid TARGETS[${i}].url. Must be a non-empty string.`);
    }
  }

  return parsed;
}

function parseStages() {
  if (STAGES_JSON) {
    let parsed;
    try {
      parsed = JSON.parse(STAGES_JSON);
    } catch (e) {
      throw new Error(`Invalid STAGES JSON: ${e.message}`);
    }
    if (!Array.isArray(parsed) || parsed.length === 0) {
      throw new Error('Invalid STAGES. Must be a non-empty JSON array.');
    }
    for (const [i, stage] of parsed.entries()) {
      if (!stage || typeof stage !== 'object') {
        throw new Error(`Invalid STAGES[${i}]. Must be an object.`);
      }
      if (typeof stage.duration !== 'string' || stage.duration.length === 0) {
        throw new Error(`Invalid STAGES[${i}].duration. Must be a non-empty string.`);
      }
      if (typeof stage.target !== 'number' || Number.isNaN(stage.target) || stage.target < 0) {
        throw new Error(`Invalid STAGES[${i}].target. Must be a non-negative number.`);
      }
    }
    return parsed;
  }

  return [
    { duration: DURATION, target: VUS },
    { duration: DURATION, target: VUS },
    { duration: DURATION, target: 0 },
  ];
}

const TARGETS = parseTargets();
const STAGES = parseStages();
const PEAK_STAGE_TARGET = STAGES.reduce((max, stage) => Math.max(max, stage.target), 0);

function resolveArrivalVUs() {
  const base = Math.max(1, Math.ceil(PEAK_STAGE_TARGET * ARRIVAL_VUS_MULTIPLIER));
  const max = Math.max(base, Math.ceil(PEAK_STAGE_TARGET * ARRIVAL_MAX_VUS_MULTIPLIER));
  return {
    preAllocatedVUs: ARRIVAL_PREALLOCATED_VUS > 0 ? ARRIVAL_PREALLOCATED_VUS : base,
    maxVUs: ARRIVAL_MAX_VUS > 0 ? ARRIVAL_MAX_VUS : max,
  };
}

function withMaxDelay(url) {
  if (!Number.isFinite(MAX_DELAY_MS) || MAX_DELAY_MS <= 0) {
    return url;
  }
  const sep = url.includes('?') ? '&' : '?';
  return `${url}${sep}max-delay=${MAX_DELAY_MS}`;
}

function randomItemId() {
  const n = Number(KEYSPACE_SIZE);
  if (!Number.isFinite(n) || n <= 0) return '0';
  return String(Math.floor(Math.random() * n));
}

function withRandomItemId(url, endpointName) {
  const id = randomItemId();
  const qIndex = url.indexOf('?');
  const base = qIndex === -1 ? url : url.slice(0, qIndex);
  const query = qIndex === -1 ? '' : url.slice(qIndex);

  if (endpointName === 'direct-hello') {
    if (base.endsWith('/')) {
      return `${base}${id}${query}`;
    }
    return `${base}/${id}${query}`;
  }

  if (base.endsWith('/hello')) {
    return `${base.slice(0, -'/hello'.length)}/${id}${query}`;
  }

  return `${base}${query}`;
}

function canonicalHeaderName(name) {
  return name
    .toLowerCase()
    .split('-')
    .filter((part) => part.length > 0)
    .map((part) => part[0].toUpperCase() + part.slice(1))
    .join('-');
}

function getHeader(res, name) {
  if (!name) return undefined;
  const candidates = [name, name.toLowerCase(), canonicalHeaderName(name)];
  for (const key of candidates) {
    const val = res.headers[key];
    if (val !== undefined) return val;
  }
  return undefined;
}

function parseBatchSize(res) {
  const raw = getHeader(res, BATCH_SIZE_HEADER);
  if (raw == null) return 1;
  const value = Array.isArray(raw) ? raw[0] : raw;
  const n = parseInt(String(value), 10);
  if (!Number.isFinite(n) || n <= 0) return 1;
  return n;
}

const thresholds = {};
for (const t of TARGETS) {
  thresholds[`http_req_duration{endpoint:${t.name}}`] = ['p(95)<2000'];
  thresholds[`http_req_failed{endpoint:${t.name}}`] = ['rate<0.1'];
}

const scenarioNameFor = (name) => `target_${name.replace(/[^a-zA-Z0-9_]/g, '_')}`;
const TARGET_BY_SCENARIO = {};
for (const t of TARGETS) {
  TARGET_BY_SCENARIO[scenarioNameFor(t.name)] = t;
}

export const options = (() => {
  const base = { thresholds, discardResponseBodies: true };
  const arrivalVus = resolveArrivalVUs();

  if (MODE === 'per_endpoint') {
    const scenarios = {};
    for (const t of TARGETS) {
      const scenarioName = scenarioNameFor(t.name);
      if (EXECUTOR === 'ramping-arrival-rate') {
        scenarios[scenarioName] = {
          executor: 'ramping-arrival-rate',
          startRate: 0,
          timeUnit: ARRIVAL_TIME_UNIT,
          stages: STAGES,
          preAllocatedVUs: arrivalVus.preAllocatedVUs,
          maxVUs: arrivalVus.maxVUs,
          exec: 'hitTarget',
        };
      } else {
        scenarios[scenarioName] = {
          executor: 'ramping-vus',
          stages: STAGES,
          exec: 'hitTarget',
        };
      }
    }
    return Object.assign({}, base, { scenarios });
  }

  if (EXECUTOR === 'ramping-arrival-rate') {
    return Object.assign({}, base, {
      scenarios: {
        batch: {
          executor: 'ramping-arrival-rate',
          startRate: 0,
          timeUnit: ARRIVAL_TIME_UNIT,
          stages: STAGES,
          preAllocatedVUs: arrivalVus.preAllocatedVUs,
          maxVUs: arrivalVus.maxVUs,
          exec: 'hitBatch',
        },
      },
    });
  }

  return Object.assign({}, base, {
    scenarios: {
      batch: {
        executor: 'ramping-vus',
        stages: STAGES,
        exec: 'hitBatch',
      },
    },
  });
})();

export function hitTarget() {
  const scenarioName = exec.scenario.name;
  const t = TARGET_BY_SCENARIO[scenarioName];
  if (!t) {
    throw new Error(`Unknown scenario '${scenarioName}' (no matching target)`);
  }

  const res = http.get(withRandomItemId(withMaxDelay(t.url), t.name), {
    tags: { endpoint: t.name, name: t.name },
  });

  check(res, {
    [`${t.name}: status is 200`]: (r) => r.status === 200,
  });

  batchSizeTrend.add(parseBatchSize(res), { endpoint: t.name, name: t.name });

  if (EXECUTOR !== 'ramping-arrival-rate') {
    sleep(0.1);
  }
}

export function hitBatch() {
  const responses = http.batch(
    TARGETS.map((t) => ({
      method: 'GET',
      url: withRandomItemId(withMaxDelay(t.url), t.name),
      params: { tags: { endpoint: t.name, name: t.name } },
    })),
  );

  for (let i = 0; i < responses.length; i++) {
    const name = TARGETS[i].name;
    check(responses[i], {
      [`${name}: status is 200`]: (r) => r.status === 200,
    });

    batchSizeTrend.add(parseBatchSize(responses[i]), { endpoint: name, name });
  }

  if (EXECUTOR !== 'ramping-arrival-rate') {
    sleep(0.1);
  }
}

export default function () {
  if (MODE === 'per_endpoint') {
    return hitTarget();
  }
  return hitBatch();
}
