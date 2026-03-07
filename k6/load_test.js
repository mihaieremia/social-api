import http from 'k6/http';
import { check } from 'k6';

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const BASE_URL = __ENV.BASE_URL || 'http://localhost:8080';

// Filter to run a single scenario: K6_SCENARIO=read_path k6 run k6/load_test.js
const ONLY_SCENARIO = __ENV.K6_SCENARIO || '';

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
const ALL_CONTENT_REFS = CONTENT_TYPES.flatMap((ct) =>
  CONTENT_IDS[ct].map((cid) => ({ content_type: ct, content_id: cid })),
);
const HOTSPOT_TOKEN = TOKENS[0];

function cycleItems(items, n) {
  const out = [];
  for (let i = 0; i < n; i++) {
    const item = items[i % items.length];
    out.push({ content_type: item.content_type, content_id: item.content_id });
  }
  return out;
}

const FIXED_BATCH_COUNTS_ITEMS = cycleItems(ALL_CONTENT_REFS, 100);
const FIXED_BATCH_STATUS_ITEMS = cycleItems(ALL_CONTENT_REFS, 100);
const DUPLICATE_HEAVY_BATCH_ITEMS = cycleItems(ALL_CONTENT_REFS.slice(0, 5), 100);

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

const ALL_SCENARIOS = {
  // --- Sequential mode (default): run one after another ---

  // Isolated: 10k rps reads
  read_path: {
    executor: 'constant-arrival-rate',
    exec: 'readCount',
    rate: 10000,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 200,
    maxVUs: 400,
    gracefulStop: '10s',
    tags: { scenario: 'read_path' },
  },

  // Isolated: 1k rps batch counts (50 items each)
  batch_path: {
    executor: 'constant-arrival-rate',
    exec: 'batchCounts',
    rate: 1000,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 50,
    maxVUs: 100,
    gracefulStop: '10s',
    startTime: '70s',
    tags: { scenario: 'batch_path' },
  },

  // Isolated: 500 rps writes (like/unlike cycling)
  write_path: {
    executor: 'constant-arrival-rate',
    exec: 'writeCycle',
    rate: 500,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 50,
    maxVUs: 100,
    gracefulStop: '10s',
    startTime: '140s',
    tags: { scenario: 'write_path' },
  },

  // Mixed: realistic 80/15/5 ratio at 2k rps
  mixed: {
    executor: 'constant-arrival-rate',
    exec: 'mixedWorkload',
    rate: 2000,
    timeUnit: '1s',
    duration: '120s',
    preAllocatedVUs: 100,
    maxVUs: 200,
    gracefulStop: '10s',
    startTime: '210s',
    tags: { scenario: 'mixed' },
  },

  // Isolated: 500 rps auth batch statuses with max-size payloads
  batch_status_path: {
    executor: 'constant-arrival-rate',
    exec: 'batchStatuses',
    rate: 500,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 50,
    maxVUs: 100,
    gracefulStop: '10s',
    startTime: '340s',
    tags: { scenario: 'batch_status_path' },
  },

  // Isolated: 250 rps identical 100-item batch counts to pressure cold/hot-spot behavior
  batch_hotspot_path: {
    executor: 'constant-arrival-rate',
    exec: 'batchHotspotCounts',
    rate: 250,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 40,
    maxVUs: 80,
    gracefulStop: '10s',
    startTime: '410s',
    tags: { scenario: 'batch_hotspot_path' },
  },

  // Isolated: 250 rps duplicate-heavy 100-item batch counts
  batch_duplicate_path: {
    executor: 'constant-arrival-rate',
    exec: 'batchDuplicateCounts',
    rate: 250,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 40,
    maxVUs: 80,
    gracefulStop: '10s',
    startTime: '480s',
    tags: { scenario: 'batch_duplicate_path' },
  },
};

// --- Parallel mode: all scenarios fire simultaneously ---
// Run with: K6_SCENARIO=parallel k6 run k6/load_test.js

const PARALLEL_SCENARIOS = {
  // Reads: 8k rps
  parallel_read: {
    executor: 'constant-arrival-rate',
    exec: 'readCount',
    rate: 8000,
    timeUnit: '1s',
    duration: '90s',
    preAllocatedVUs: 200,
    maxVUs: 400,
    gracefulStop: '10s',
    tags: { scenario: 'parallel_read' },
  },

  // Batch: 500 rps
  parallel_batch: {
    executor: 'constant-arrival-rate',
    exec: 'batchCounts',
    rate: 500,
    timeUnit: '1s',
    duration: '90s',
    preAllocatedVUs: 50,
    maxVUs: 100,
    gracefulStop: '10s',
    tags: { scenario: 'parallel_batch' },
  },

  // Hot-spot batch: identical 100-item payload from many VUs at once
  parallel_batch_hotspot: {
    executor: 'constant-arrival-rate',
    exec: 'batchHotspotCounts',
    rate: 200,
    timeUnit: '1s',
    duration: '90s',
    preAllocatedVUs: 40,
    maxVUs: 80,
    gracefulStop: '10s',
    tags: { scenario: 'parallel_batch_hotspot' },
  },

  // Writes: 300 rps (like/unlike cycling)
  parallel_write: {
    executor: 'constant-arrival-rate',
    exec: 'writeCycle',
    rate: 300,
    timeUnit: '1s',
    duration: '90s',
    preAllocatedVUs: 50,
    maxVUs: 100,
    gracefulStop: '10s',
    tags: { scenario: 'parallel_write' },
  },

  // SSE: ramp connections from 0 to target over 30s, hold, then drain
  parallel_sse: {
    executor: 'ramping-vus',
    exec: 'sseSubscribe',
    startVUs: 0,
    stages: [
      { duration: '15s', target: 200 },  // ramp up to 200 connections
      { duration: '60s', target: 200 },  // hold 200 SSE connections
      { duration: '15s', target: 0 },    // drain
    ],
    gracefulRampDown: '10s',
    gracefulStop: '10s',
    tags: { scenario: 'parallel_sse' },
  },
};

// --- SSE stress test: push connections to the limit ---
// Run with: K6_SCENARIO=sse_stress k6 run k6/load_test.js

const SSE_STRESS_SCENARIOS = {
  // Background writes to generate real SSE events.
  // Duration covers the full SSE ramp cycle (15+45+15+45+15+45+15 = 195s).
  sse_stress_writes: {
    executor: 'constant-arrival-rate',
    exec: 'writeCycle',
    rate: 200,
    timeUnit: '1s',
    duration: '195s',
    preAllocatedVUs: 30,
    maxVUs: 60,
    gracefulStop: '10s',
    tags: { scenario: 'sse_stress_writes' },
  },

  // SSE connections: ramp 0 -> 500 -> 1000 -> 2000, hold, drain
  // Each VU holds a 35s SSE connection. gracefulRampDown/Stop must exceed
  // that so in-flight connections can finish without being force-killed.
  sse_stress_connections: {
    executor: 'ramping-vus',
    exec: 'sseSubscribe',
    startVUs: 0,
    stages: [
      { duration: '15s', target: 500 },
      { duration: '45s', target: 500 },
      { duration: '15s', target: 1000 },
      { duration: '45s', target: 1000 },
      { duration: '15s', target: 2000 },
      { duration: '45s', target: 2000 },
      { duration: '15s', target: 0 },
    ],
    gracefulRampDown: '40s',
    gracefulStop: '40s',
    tags: { scenario: 'sse_stress_connections' },
  },
};

// ---------------------------------------------------------------------------
// Scenario selector
// ---------------------------------------------------------------------------

function buildScenarios() {
  if (ONLY_SCENARIO === 'parallel') {
    return PARALLEL_SCENARIOS;
  }
  if (ONLY_SCENARIO === 'sse_stress') {
    return SSE_STRESS_SCENARIOS;
  }
  if (ONLY_SCENARIO && ALL_SCENARIOS[ONLY_SCENARIO]) {
    const s = Object.assign({}, ALL_SCENARIOS[ONLY_SCENARIO]);
    delete s.startTime;
    return { [ONLY_SCENARIO]: s };
  }
  return ALL_SCENARIOS;
}

function buildThresholds() {
  if (ONLY_SCENARIO === 'parallel') {
    return {
      'http_req_duration{scenario:parallel_read}': ['p(99)<15'],
      'http_req_duration{scenario:parallel_batch}': ['p(99)<75'],
      'http_req_duration{scenario:parallel_batch_hotspot}': ['p(99)<100'],
      'http_req_duration{scenario:parallel_write}': ['p(99)<150'],
      // Exclude SSE from error rate — timeouts are expected for streaming
      'http_req_failed{scenario:parallel_read}': ['rate<0.01'],
      'http_req_failed{scenario:parallel_batch}': ['rate<0.01'],
      'http_req_failed{scenario:parallel_batch_hotspot}': ['rate<0.01'],
      'http_req_failed{scenario:parallel_write}': ['rate<0.01'],
    };
  }
  if (ONLY_SCENARIO === 'sse_stress') {
    return {
      'http_req_failed{scenario:sse_stress_writes}': ['rate<0.05'],
    };
  }
  return {
    'http_req_duration{scenario:read_path}': ['p(99)<5'],
    'http_req_duration{scenario:batch_path}': ['p(99)<50'],
    'http_req_duration{scenario:batch_status_path}': ['p(99)<75'],
    'http_req_duration{scenario:batch_hotspot_path}': ['p(99)<100'],
    'http_req_duration{scenario:batch_duplicate_path}': ['p(99)<100'],
    'http_req_duration{scenario:write_path}': ['p(99)<100'],
    'http_req_duration{scenario:mixed}': ['p(99)<100'],
    http_req_failed: ['rate<0.01'],
  };
}

export const options = {
  scenarios: buildScenarios(),
  thresholds: buildThresholds(),
};

// ---------------------------------------------------------------------------
// Scenario functions
// ---------------------------------------------------------------------------

// Read: GET /v1/likes/{type}/{id}/count
export function readCount() {
  const { content_type, content_id } = randomContent();
  const url = `${BASE_URL}/v1/likes/${content_type}/${content_id}/count`;

  const res = http.get(url, { tags: { name: 'GET_count' } });

  check(res, {
    'read: status ok': (r) => r.status === 200 || r.status === 429,
    'read: has count': (r) => {
      if (r.status === 429) return true;
      try {
        return JSON.parse(r.body).count !== undefined;
      } catch (_) {
        return false;
      }
    },
  });
}

// Batch: POST /v1/likes/batch/counts with 50 items
export function batchCounts() {
  const items = randomBatchItems(50);
  const expectedLength = 50;
  const url = `${BASE_URL}/v1/likes/batch/counts`;
  const payload = JSON.stringify({ items });

  const res = http.post(url, payload, {
    headers: JSON_HEADERS,
    tags: { name: 'POST_batch_counts' },
  });

  check(res, {
    'batch: status ok': (r) => r.status === 200 || r.status === 429,
    'batch: results length': (r) => {
      if (r.status === 429) return true;
      try {
        return JSON.parse(r.body).results.length === expectedLength;
      } catch (_) {
        return false;
      }
    },
  });
}

// Batch statuses: POST /v1/likes/batch/statuses with 100 items
export function batchStatuses() {
  const items = FIXED_BATCH_STATUS_ITEMS;
  const url = `${BASE_URL}/v1/likes/batch/statuses`;
  const payload = JSON.stringify({ items });

  const res = http.post(url, payload, {
    headers: authHeaders(HOTSPOT_TOKEN),
    tags: { name: 'POST_batch_statuses' },
  });

  check(res, {
    'batch-statuses: status ok': (r) => r.status === 200 || r.status === 429,
    'batch-statuses: results length': (r) => {
      if (r.status === 429) return true;
      try {
        return JSON.parse(r.body).results.length === 100;
      } catch (_) {
        return false;
      }
    },
  });
}

// Hot-spot batch counts: identical 100-item payload from all VUs
export function batchHotspotCounts() {
  const url = `${BASE_URL}/v1/likes/batch/counts`;
  const payload = JSON.stringify({ items: FIXED_BATCH_COUNTS_ITEMS });

  const res = http.post(url, payload, {
    headers: JSON_HEADERS,
    tags: { name: 'POST_batch_counts_hotspot' },
  });

  check(res, {
    'batch-hotspot: status ok': (r) => r.status === 200 || r.status === 429,
    'batch-hotspot: results length': (r) => {
      if (r.status === 429) return true;
      try {
        return JSON.parse(r.body).results.length === 100;
      } catch (_) {
        return false;
      }
    },
  });
}

// Duplicate-heavy batch counts: 100 items built from only 5 unique refs
export function batchDuplicateCounts() {
  const url = `${BASE_URL}/v1/likes/batch/counts`;
  const payload = JSON.stringify({ items: DUPLICATE_HEAVY_BATCH_ITEMS });

  const res = http.post(url, payload, {
    headers: JSON_HEADERS,
    tags: { name: 'POST_batch_counts_duplicate' },
  });

  check(res, {
    'batch-duplicate: status ok': (r) => r.status === 200 || r.status === 429,
    'batch-duplicate: results length': (r) => {
      if (r.status === 429) return true;
      try {
        return JSON.parse(r.body).results.length === 100;
      } catch (_) {
        return false;
      }
    },
  });
}

// Write: alternate like/unlike per iteration
// Each VU picks a stable (token, content) pair and cycles like/unlike.
export function writeCycle() {
  const vuId = __VU;
  const iter = __ITER;

  // Deterministic assignment: each VU gets a stable token and content pair
  const token = TOKENS[vuId % TOKENS.length];
  const ctIdx = vuId % CONTENT_TYPES.length;
  const ct = CONTENT_TYPES[ctIdx];
  const ids = CONTENT_IDS[ct];
  const cid = ids[Math.floor(vuId / CONTENT_TYPES.length) % ids.length];

  if (iter % 2 === 0) {
    // Like
    const url = `${BASE_URL}/v1/likes`;
    const payload = JSON.stringify({ content_type: ct, content_id: cid });
    const res = http.post(url, payload, {
      headers: authHeaders(token),
      tags: { name: 'POST_like' },
    });

    check(res, {
      'write-like: status ok': (r) => r.status === 201 || r.status === 429,
      'write-like: has liked': (r) => {
        if (r.status === 429) return true;
        try {
          return JSON.parse(r.body).liked !== undefined;
        } catch (_) {
          return false;
        }
      },
    });
  } else {
    // Unlike
    const url = `${BASE_URL}/v1/likes/${ct}/${cid}`;
    const res = http.del(url, null, {
      headers: authHeaders(token),
      tags: { name: 'DELETE_unlike' },
    });

    check(res, {
      'write-unlike: status ok': (r) => r.status === 200 || r.status === 429,
      'write-unlike: has was_liked': (r) => {
        if (r.status === 429) return true;
        try {
          return JSON.parse(r.body).was_liked !== undefined;
        } catch (_) {
          return false;
        }
      },
    });
  }
}

// Mixed: 80% read, 15% batch, 5% write
export function mixedWorkload() {
  const roll = Math.random();

  if (roll < 0.80) {
    readCount();
  } else if (roll < 0.95) {
    batchCounts();
  } else {
    writeCycle();
  }
}

// SSE: open a connection, hold it for a duration, then close.
// k6's http.get() blocks until timeout, which is exactly what we want —
// each VU holds one SSE connection for the timeout period.
// The real metric is social_api_sse_connections_active in Grafana.
//
// Timeout vs heartbeat:
//   Server heartbeat interval = 10s (SSE_HEARTBEAT_INTERVAL_SECS).
//   k6 timeout = 35s → guarantees at least 2-3 heartbeats per connection.
//   k6 reports "request timeout" warnings when the timeout fires — this is
//   expected for SSE and NOT an error. The timeout is the mechanism for
//   cycling connections during the stress test.
export function sseSubscribe() {
  const { content_type, content_id } = randomContent();
  const url = `${BASE_URL}/v1/likes/stream?content_type=${content_type}&content_id=${content_id}`;

  // Hold the connection open for 35s — outlives at least 2 server heartbeats (10s each).
  // k6 will log "request timeout" when this fires; that's expected for SSE streams.
  const res = http.get(url, {
    tags: { name: 'GET_sse_stream' },
    timeout: '35s',
    responseType: 'text',
  });

  // k6 sets status=0 on timeout for streaming responses, so check body instead.
  // With 10s heartbeat interval and 35s timeout, we should always get heartbeats.
  check(res, {
    'sse: received heartbeat or event': (r) =>
      r.body && r.body.includes('event'),
  });
}

// Default export for CLI override usage (k6 run --vus 5 --duration 10s)
export default function () {
  mixedWorkload();
}
