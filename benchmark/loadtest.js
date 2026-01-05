import http from 'k6/http';
import { check, sleep } from 'k6';
import exec from 'k6/execution';

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

  if (!Array.isArray(parsed) || parsed.length < 2) {
    throw new Error('Invalid TARGETS. Must be a JSON array with at least 2 items.');
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
const PEAK_STAGE_TARGET = Math.max(...STAGES.map((stage) => stage.target));

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
    return { ...base, scenarios };
  }

  if (EXECUTOR === 'ramping-arrival-rate') {
    return {
      ...base,
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
    };
  }

  return {
    ...base,
    scenarios: {
      batch: {
        executor: 'ramping-vus',
        stages: STAGES,
        exec: 'hitBatch',
      },
    },
  };
})();

export function hitTarget() {
  const scenarioName = exec.scenario.name;
  const t = TARGET_BY_SCENARIO[scenarioName];
  if (!t) {
    throw new Error(`Unknown scenario '${scenarioName}' (no matching target)`);
  }

  const res = http.get(withMaxDelay(t.url), {
    tags: { endpoint: t.name, name: t.name },
  });

  check(res, {
    [`${t.name}: status is 200`]: (r) => r.status === 200,
  });

  if (EXECUTOR !== 'ramping-arrival-rate') {
    sleep(0.1);
  }
}

export function hitBatch() {
  const responses = http.batch(
    TARGETS.map((t) => ({
      method: 'GET',
      url: withMaxDelay(t.url),
      params: { tags: { endpoint: t.name, name: t.name } },
    })),
  );

  for (let i = 0; i < responses.length; i++) {
    const name = TARGETS[i].name;
    check(responses[i], {
      [`${name}: status is 200`]: (r) => r.status === 200,
    });
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
