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
    startTime: '70s', // after read_path finishes
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
    startTime: '140s', // after batch_path finishes
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
    startTime: '210s', // after write_path finishes
    tags: { scenario: 'mixed' },
  },
};

// Filter to single scenario if K6_SCENARIO is set.
// When running a single scenario, strip startTime so it begins immediately.
function buildScenarios() {
  if (ONLY_SCENARIO && ALL_SCENARIOS[ONLY_SCENARIO]) {
    const s = Object.assign({}, ALL_SCENARIOS[ONLY_SCENARIO]);
    delete s.startTime;
    return { [ONLY_SCENARIO]: s };
  }
  return ALL_SCENARIOS;
}

export const options = {
  scenarios: buildScenarios(),

  thresholds: {
    // Per-scenario p99 latency
    'http_req_duration{scenario:read_path}': ['p(99)<10'],
    'http_req_duration{scenario:batch_path}': ['p(99)<50'],
    'http_req_duration{scenario:write_path}': ['p(99)<100'],
    'http_req_duration{scenario:mixed}': ['p(99)<100'],

    // Global error rate < 1%
    http_req_failed: ['rate<0.01'],
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

  check(res, {
    'read: status ok': (r) => r.status === 200 || r.status === 429,
    'read: has count': (r) => {
      if (r.status === 429) return true; // rate limited is expected under load
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
        return JSON.parse(r.body).results.length === 50;
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

// Default export for CLI override usage (k6 run --vus 5 --duration 10s)
export default function () {
  mixedWorkload();
}
