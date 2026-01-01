"use strict";

function batchAdapter(userHandler, options = {}) {
  if (typeof userHandler !== "function") {
    throw new TypeError("batchAdapter(userHandler): userHandler must be a function");
  }
  const concurrency = options.concurrency ?? 16;

  return async function handler(event) {
    const batch = Array.isArray(event?.batch) ? event.batch : [];
    const responses = [];

    // Minimal implementation: sequential execution in scaffold; real concurrency added later.
    for (const item of batch) {
      // eslint-disable-next-line no-await-in-loop
      const resp = await userHandler(item);
      responses.push({ id: item.id, ...resp });
    }

    return { v: 1, responses };
  };
}

module.exports = { batchAdapter };

