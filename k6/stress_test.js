import http from 'k6/http';
import { check, sleep } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const BASE_URL = __ENV.BASE_URL || 'http://localhost:8080';
const SCENARIO = __ENV.K6_SCENARIO || 'stress';

// Duration for sustained phase (default 30m)
const SUSTAIN_DURATION = __ENV.STRESS_DURATION || '30m';

// Target RPS for stress test (default 100k)
const TARGET_RPS = parseInt(__ENV.STRESS_TARGET_RPS || '100000');

// 20 auth tokens matching mock-services data.rs
const TOKENS = Array.from({ length: 20 }, (_, i) => `tok_user_${i + 1}`);

// 20 content IDs per type matching mock-services data.rs
const CONTENT_IDS = {
  post: [
    '731b0395-4888-4822-b516-05b4b7bf2089',
    '9601c044-6130-4ee5-a155-96570e05a02f',
    '933dde0f-4744-4a66-9a38-bf5cb1f67553',
    'ea0f2020-0509-45fd-adb9-24b8843055ee',
    'bd27f926-0a00-41fd-b085-a7491e6d0902',
    'a0000001-0001-4000-8000-000000000001',
    'a0000001-0002-4000-8000-000000000002',
    'a0000001-0003-4000-8000-000000000003',
    'a0000001-0004-4000-8000-000000000004',
    'a0000001-0005-4000-8000-000000000005',
    'a0000001-0006-4000-8000-000000000006',
    'a0000001-0007-4000-8000-000000000007',
    'a0000001-0008-4000-8000-000000000008',
    'a0000001-0009-4000-8000-000000000009',
    'a0000001-000a-4000-8000-00000000000a',
    'a0000001-000b-4000-8000-00000000000b',
    'a0000001-000c-4000-8000-00000000000c',
    'a0000001-000d-4000-8000-00000000000d',
    'a0000001-000e-4000-8000-00000000000e',
    'a0000001-000f-4000-8000-00000000000f',
  ],
  bonus_hunter: [
    'c3d4e5f6-a7b8-9012-cdef-123456789012',
    'd4e5f6a7-b8c9-0123-def0-234567890123',
    'e5f6a7b8-c9d0-1234-ef01-345678901234',
    'f6a7b8c9-d0e1-2345-f012-456789012345',
    'a7b8c9d0-e1f2-3456-0123-567890123456',
    'b0000002-0001-4000-8000-000000000001',
    'b0000002-0002-4000-8000-000000000002',
    'b0000002-0003-4000-8000-000000000003',
    'b0000002-0004-4000-8000-000000000004',
    'b0000002-0005-4000-8000-000000000005',
    'b0000002-0006-4000-8000-000000000006',
    'b0000002-0007-4000-8000-000000000007',
    'b0000002-0008-4000-8000-000000000008',
    'b0000002-0009-4000-8000-000000000009',
    'b0000002-000a-4000-8000-00000000000a',
    'b0000002-000b-4000-8000-00000000000b',
    'b0000002-000c-4000-8000-00000000000c',
    'b0000002-000d-4000-8000-00000000000d',
    'b0000002-000e-4000-8000-00000000000e',
    'b0000002-000f-4000-8000-00000000000f',
  ],
  top_picks: [
    'b8c9d0e1-f2a3-4567-1234-678901234567',
    'c9d0e1f2-a3b4-5678-2345-789012345678',
    'd0e1f2a3-b4c5-6789-3456-890123456789',
    'e1f2a3b4-c5d6-7890-4567-901234567890',
    'f2a3b4c5-d6e7-8901-5678-012345678901',
    'c0000003-0001-4000-8000-000000000001',
    'c0000003-0002-4000-8000-000000000002',
    'c0000003-0003-4000-8000-000000000003',
    'c0000003-0004-4000-8000-000000000004',
    'c0000003-0005-4000-8000-000000000005',
    'c0000003-0006-4000-8000-000000000006',
    'c0000003-0007-4000-8000-000000000007',
    'c0000003-0008-4000-8000-000000000008',
    'c0000003-0009-4000-8000-000000000009',
    'c0000003-000a-4000-8000-00000000000a',
    'c0000003-000b-4000-8000-00000000000b',
    'c0000003-000c-4000-8000-00000000000c',
    'c0000003-000d-4000-8000-00000000000d',
    'c0000003-000e-4000-8000-00000000000e',
    'c0000003-000f-4000-8000-00000000000f',
  ],
};

const CONTENT_TYPES = Object.keys(CONTENT_IDS);

// ---------------------------------------------------------------------------
// Custom metrics
// ---------------------------------------------------------------------------

const errorsByType = new Counter('stress_errors_by_type');
const degradedResponses = new Rate('stress_degraded_responses');
const readLatency = new Trend('stress_read_latency', true);
const writeLatency = new Trend('stress_write_latency', true);
const batchLatency = new Trend('stress_batch_latency', true);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function randomItem(arr) {
  return arr[Math.floor(Math.random() * arr.length)];
}

function randomToken() {
  return randomItem(TOKENS);
}

function randomContent() {
  const ct = randomItem(CONTENT_TYPES);
  const cid = randomItem(CONTENT_IDS[ct]);
  return { content_type: ct, content_id: cid };
}

function randomBatchItems(n) {
  const items = [];
  for (let i = 0; i < n; i++) {
    items.push(randomContent());
  }
  return items;
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

function authHeaders(token) {
  return {
    'Content-Type': 'application/json',
    Authorization: `Bearer ${token}`,
  };
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

// VU math for constant-arrival-rate:
//   VUs needed = target_rps * avg_response_time_seconds
//   At 100k rps with 10ms avg  → 1,000 VUs
//   At 100k rps with 50ms avg  → 5,000 VUs
//   At 100k rps with 200ms avg → 20,000 VUs (degraded)
//
// IMPORTANT: macOS has ~16K ephemeral ports (49152-65535) by default.
// Even with k6-tune-macos expanding to ~64K ports, high VU counts cause
// port exhaustion because TIME_WAIT sockets hold ports for seconds.
// We cap total VUs to stay well within OS limits on localhost.
// In production (Linux + separate load generator), remove the cap.

// Max VUs per scenario (override: STRESS_MAX_VUS env var)
// Default 2000 keeps total across 3 scenarios at ~5K, safe for macOS.
const MAX_VUS_PER_SCENARIO = parseInt(__ENV.STRESS_MAX_VUS || '2000');

// Helper: proportional RPS based on target
function rps(fraction) {
  return Math.max(1, Math.round(TARGET_RPS * fraction));
}

// Helper: VU allocation based on RPS, capped to avoid port exhaustion
function vusFor(targetRps) {
  const pre = Math.max(10, Math.round(targetRps * 0.05));
  const max = Math.max(50, Math.round(targetRps * 0.2));
  return {
    preAllocatedVUs: Math.min(pre, Math.round(MAX_VUS_PER_SCENARIO * 0.25)),
    maxVUs: Math.min(max, MAX_VUS_PER_SCENARIO),
  };
}

// --- stress: Progressive ramp to 100k RPS, hold 30m, cool down ---
//
// Timeline:
//   0-2m    warm-up at 10% (10k rps)
//   2-4m    ramp to 25% (25k rps)
//   4-6m    ramp to 50% (50k rps)
//   6-8m    ramp to 75% (75k rps)
//   8-10m   ramp to 100% (100k rps)
//   10-40m  SUSTAIN at 100% (100k rps for 30m)
//   40-42m  cool-down to 0
//
// We use ramping-arrival-rate for smooth transitions.

const STRESS_SCENARIOS = {
  // Reads: 80% of total RPS
  stress_read: {
    executor: 'ramping-arrival-rate',
    exec: 'readCount',
    startRate: rps(0.08),            // 10% of 80% = 8% total
    timeUnit: '1s',
    stages: [
      { duration: '2m', target: rps(0.08) },    // warm-up: 8k rps
      { duration: '2m', target: rps(0.20) },     // ramp: 20k rps
      { duration: '2m', target: rps(0.40) },     // ramp: 40k rps
      { duration: '2m', target: rps(0.60) },     // ramp: 60k rps
      { duration: '2m', target: rps(0.80) },     // ramp: 80k rps
      { duration: SUSTAIN_DURATION, target: rps(0.80) }, // SUSTAIN: 80k rps
      { duration: '2m', target: 0 },             // cool-down
    ],
    ...vusFor(rps(0.80)),
    gracefulStop: '30s',
    tags: { scenario: 'stress_read' },
  },

  // Batch: 10% of total RPS (each batch = 20 items for speed)
  stress_batch: {
    executor: 'ramping-arrival-rate',
    exec: 'batchSmall',
    startRate: rps(0.01),
    timeUnit: '1s',
    stages: [
      { duration: '2m', target: rps(0.01) },
      { duration: '2m', target: rps(0.025) },
      { duration: '2m', target: rps(0.05) },
      { duration: '2m', target: rps(0.075) },
      { duration: '2m', target: rps(0.10) },
      { duration: SUSTAIN_DURATION, target: rps(0.10) },
      { duration: '2m', target: 0 },
    ],
    ...vusFor(rps(0.10)),
    gracefulStop: '30s',
    tags: { scenario: 'stress_batch' },
  },

  // Writes: 10% of total RPS (like/unlike cycling)
  stress_write: {
    executor: 'ramping-arrival-rate',
    exec: 'writeCycle',
    startRate: rps(0.01),
    timeUnit: '1s',
    stages: [
      { duration: '2m', target: rps(0.01) },
      { duration: '2m', target: rps(0.025) },
      { duration: '2m', target: rps(0.05) },
      { duration: '2m', target: rps(0.075) },
      { duration: '2m', target: rps(0.10) },
      { duration: SUSTAIN_DURATION, target: rps(0.10) },
      { duration: '2m', target: 0 },
    ],
    ...vusFor(rps(0.10)),
    gracefulStop: '30s',
    tags: { scenario: 'stress_write' },
  },
};

// --- spike: Sudden burst from idle to 100k RPS ---
// Tests how the system handles a sudden traffic spike (e.g., viral content).

const SPIKE_SCENARIOS = {
  spike_read: {
    executor: 'ramping-arrival-rate',
    exec: 'readCount',
    startRate: rps(0.008),  // ~800 rps baseline
    timeUnit: '1s',
    stages: [
      { duration: '1m', target: rps(0.008) },   // quiet baseline
      { duration: '10s', target: rps(0.80) },    // SPIKE to 80k rps in 10s
      { duration: '5m', target: rps(0.80) },     // hold spike
      { duration: '30s', target: rps(0.008) },   // drop back
      { duration: '2m', target: rps(0.008) },    // recovery observation
      { duration: '10s', target: rps(0.80) },    // SPIKE again (tests recovery)
      { duration: '5m', target: rps(0.80) },     // hold
      { duration: '1m', target: 0 },             // cool-down
    ],
    ...vusFor(rps(0.80)),
    gracefulStop: '30s',
    tags: { scenario: 'spike_read' },
  },

  spike_write: {
    executor: 'ramping-arrival-rate',
    exec: 'writeCycle',
    startRate: rps(0.001),
    timeUnit: '1s',
    stages: [
      { duration: '1m', target: rps(0.001) },
      { duration: '10s', target: rps(0.10) },
      { duration: '5m', target: rps(0.10) },
      { duration: '30s', target: rps(0.001) },
      { duration: '2m', target: rps(0.001) },
      { duration: '10s', target: rps(0.10) },
      { duration: '5m', target: rps(0.10) },
      { duration: '1m', target: 0 },
    ],
    ...vusFor(rps(0.10)),
    gracefulStop: '30s',
    tags: { scenario: 'spike_write' },
  },
};

// --- soak: Moderate load for extended period (find memory leaks, connection leaks) ---
// 50% of target RPS for 30m. Focus: stability, not peak throughput.

const SOAK_SCENARIOS = {
  soak_read: {
    executor: 'constant-arrival-rate',
    exec: 'readCount',
    rate: rps(0.40),      // 40k rps
    timeUnit: '1s',
    duration: SUSTAIN_DURATION,
    ...vusFor(rps(0.40)),
    gracefulStop: '30s',
    tags: { scenario: 'soak_read' },
  },

  soak_batch: {
    executor: 'constant-arrival-rate',
    exec: 'batchSmall',
    rate: rps(0.05),      // 5k rps
    timeUnit: '1s',
    duration: SUSTAIN_DURATION,
    ...vusFor(rps(0.05)),
    gracefulStop: '30s',
    tags: { scenario: 'soak_batch' },
  },

  soak_write: {
    executor: 'constant-arrival-rate',
    exec: 'writeCycle',
    rate: rps(0.05),      // 5k rps
    timeUnit: '1s',
    duration: SUSTAIN_DURATION,
    ...vusFor(rps(0.05)),
    gracefulStop: '30s',
    tags: { scenario: 'soak_write' },
  },

  // SSE connections held throughout to check for leaks
  soak_sse: {
    executor: 'ramping-vus',
    exec: 'sseSubscribe',
    startVUs: 0,
    stages: [
      { duration: '2m', target: 500 },
      { duration: SUSTAIN_DURATION, target: 500 },
      { duration: '1m', target: 0 },
    ],
    gracefulRampDown: '15s',
    gracefulStop: '15s',
    tags: { scenario: 'soak_sse' },
  },
};

// --- breakpoint: Find the breaking point by ramping until failure ---
// Starts at 1k rps, increases 10k every 2 minutes until error rate > 5%

const BREAKPOINT_SCENARIOS = {
  breakpoint_mixed: {
    executor: 'ramping-arrival-rate',
    exec: 'mixedWorkload',
    startRate: 1000,
    timeUnit: '1s',
    stages: [
      { duration: '2m', target: 1000 },
      { duration: '2m', target: 10000 },
      { duration: '2m', target: 20000 },
      { duration: '2m', target: 30000 },
      { duration: '2m', target: 40000 },
      { duration: '2m', target: 50000 },
      { duration: '2m', target: 60000 },
      { duration: '2m', target: 70000 },
      { duration: '2m', target: 80000 },
      { duration: '2m', target: 90000 },
      { duration: '2m', target: 100000 },
      { duration: '5m', target: 100000 },  // hold at max
      { duration: '2m', target: 0 },
    ],
    preAllocatedVUs: Math.min(500, Math.round(MAX_VUS_PER_SCENARIO * 0.25)),
    maxVUs: MAX_VUS_PER_SCENARIO,
    gracefulStop: '30s',
    tags: { scenario: 'breakpoint_mixed' },
  },
};

// ---------------------------------------------------------------------------
// Scenario selector
// ---------------------------------------------------------------------------

function buildScenarios() {
  switch (SCENARIO) {
    case 'stress':
      return STRESS_SCENARIOS;
    case 'spike':
      return SPIKE_SCENARIOS;
    case 'soak':
      return SOAK_SCENARIOS;
    case 'breakpoint':
      return BREAKPOINT_SCENARIOS;
    default:
      console.error(`Unknown scenario: ${SCENARIO}`);
      console.error('Available: stress, spike, soak, breakpoint');
      return STRESS_SCENARIOS;
  }
}

function buildThresholds() {
  // Stress tests use looser thresholds — we expect degradation at 100k rps.
  // The goal is observability, not green checks.
  switch (SCENARIO) {
    case 'stress':
      return {
        'http_req_duration{scenario:stress_read}': ['p(95)<100', 'p(99)<500'],
        'http_req_duration{scenario:stress_batch}': ['p(95)<300', 'p(99)<1000'],
        'http_req_duration{scenario:stress_write}': ['p(95)<500', 'p(99)<2000'],
        'http_req_failed{scenario:stress_read}': ['rate<0.05'],
        'http_req_failed{scenario:stress_batch}': ['rate<0.05'],
        'http_req_failed{scenario:stress_write}': ['rate<0.10'],
      };
    case 'spike':
      return {
        // During spike, we expect some degradation but should recover
        'http_req_failed{scenario:spike_read}': ['rate<0.10'],
        'http_req_failed{scenario:spike_write}': ['rate<0.15'],
      };
    case 'soak':
      return {
        // Soak test: tighter thresholds since load is moderate
        'http_req_duration{scenario:soak_read}': ['p(95)<30', 'p(99)<100'],
        'http_req_duration{scenario:soak_batch}': ['p(95)<100', 'p(99)<300'],
        'http_req_duration{scenario:soak_write}': ['p(95)<200', 'p(99)<500'],
        'http_req_failed{scenario:soak_read}': ['rate<0.01'],
        'http_req_failed{scenario:soak_batch}': ['rate<0.01'],
        'http_req_failed{scenario:soak_write}': ['rate<0.02'],
      };
    case 'breakpoint':
      return {
        // No strict thresholds — the point is finding where things break.
        // Just track that we don't immediately fall over.
        'http_req_failed{scenario:breakpoint_mixed}': ['rate<0.50'],
      };
    default:
      return {};
  }
}

export const options = {
  scenarios: buildScenarios(),
  thresholds: buildThresholds(),

  // Disable default summary thresholds to reduce noise at high RPS
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(90)', 'p(95)', 'p(99)', 'count'],

  // Don't abort on high error rate for stress/breakpoint — we want to see how far it goes
  noConnectionReuse: false,

  // Batch HTTP for better throughput
  batch: 20,
  batchPerHost: 20,

  // DNS caching (avoid DNS overhead at high RPS)
  dns: {
    ttl: '5m',
    select: 'roundRobin',
  },
};

// ---------------------------------------------------------------------------
// Scenario functions
// ---------------------------------------------------------------------------

// Read: GET /v1/likes/{type}/{id}/count
export function readCount() {
  const { content_type, content_id } = randomContent();
  const url = `${BASE_URL}/v1/likes/${content_type}/${content_id}/count`;

  const res = http.get(url, { tags: { name: 'GET_count' } });

  readLatency.add(res.timings.duration);

  const ok = res.status === 200 || res.status === 429;
  if (!ok) {
    errorsByType.add(1, { type: 'read', status: String(res.status) });
  }
  degradedResponses.add(res.timings.duration > 100);

  check(res, {
    'read: status ok': (r) => r.status === 200 || r.status === 429,
  });
}

// Batch: POST /v1/likes/batch/counts with 20 items (smaller for stress)
export function batchSmall() {
  const items = randomBatchItems(20);
  const url = `${BASE_URL}/v1/likes/batch/counts`;
  const payload = JSON.stringify({ items });

  const res = http.post(url, payload, {
    headers: JSON_HEADERS,
    tags: { name: 'POST_batch_counts' },
  });

  batchLatency.add(res.timings.duration);

  const ok = res.status === 200 || res.status === 429;
  if (!ok) {
    errorsByType.add(1, { type: 'batch', status: String(res.status) });
  }
  degradedResponses.add(res.timings.duration > 300);

  check(res, {
    'batch: status ok': (r) => r.status === 200 || r.status === 429,
  });
}

// Write: alternate like/unlike per iteration
export function writeCycle() {
  const vuId = __VU;
  const iter = __ITER;

  const token = TOKENS[vuId % TOKENS.length];
  const ctIdx = vuId % CONTENT_TYPES.length;
  const ct = CONTENT_TYPES[ctIdx];
  const ids = CONTENT_IDS[ct];
  const cid = ids[Math.floor(vuId / CONTENT_TYPES.length) % ids.length];

  if (iter % 2 === 0) {
    const url = `${BASE_URL}/v1/likes`;
    const payload = JSON.stringify({ content_type: ct, content_id: cid });
    const res = http.post(url, payload, {
      headers: authHeaders(token),
      tags: { name: 'POST_like' },
    });

    writeLatency.add(res.timings.duration);

    const ok = res.status === 201 || res.status === 429;
    if (!ok) {
      errorsByType.add(1, { type: 'write_like', status: String(res.status) });
    }

    check(res, {
      'write-like: status ok': (r) => r.status === 201 || r.status === 429,
    });
  } else {
    const url = `${BASE_URL}/v1/likes/${ct}/${cid}`;
    const res = http.del(url, null, {
      headers: authHeaders(token),
      tags: { name: 'DELETE_unlike' },
    });

    writeLatency.add(res.timings.duration);

    const ok = res.status === 200 || res.status === 429;
    if (!ok) {
      errorsByType.add(1, { type: 'write_unlike', status: String(res.status) });
    }

    check(res, {
      'write-unlike: status ok': (r) => r.status === 200 || r.status === 429,
    });
  }
}

// Mixed: 80% read, 15% batch, 5% write (for breakpoint test)
export function mixedWorkload() {
  const roll = Math.random();
  if (roll < 0.80) {
    readCount();
  } else if (roll < 0.95) {
    batchSmall();
  } else {
    writeCycle();
  }
}

// SSE: hold connection (for soak test)
export function sseSubscribe() {
  const { content_type, content_id } = randomContent();
  const url = `${BASE_URL}/v1/likes/stream?content_type=${content_type}&content_id=${content_id}`;

  const res = http.get(url, {
    tags: { name: 'GET_sse_stream' },
    timeout: '30s',
    responseType: 'text',
  });

  check(res, {
    'sse: received data': (r) => r.body && r.body.includes('event'),
  });
}

// Default: mixed workload
export default function () {
  mixedWorkload();
}
