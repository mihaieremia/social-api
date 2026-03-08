import http from 'k6/http';
import grpc from 'k6/net/grpc';
import { check, sleep } from 'k6';

// ---------------------------------------------------------------------------
// Quick smoke test covering HTTP + gRPC + SSE to populate all dashboard panels.
// Usage: k6 run k6/quick_all_transports.js
// ---------------------------------------------------------------------------

const BASE_URL = __ENV.BASE_URL || 'http://localhost:8080';
const GRPC_HOST = __ENV.GRPC_HOST || 'localhost:50051';
const PROTO_DIR = __ENV.PROTO_DIR || '../proto';

const TOKENS = Array.from({ length: 20 }, (_, i) => `tok_user_${i + 1}`);
const CONTENT_IDS = {
  post: [
    '731b0395-4888-4822-b516-05b4b7bf2089',
    '9601c044-6130-4ee5-a155-96570e05a02f',
    '933dde0f-4744-4a66-9a38-bf5cb1f67553',
    'ea0f2020-0509-45fd-adb9-24b8843055ee',
    'bd27f926-0a00-41fd-b085-a7491e6d0902',
  ],
  bonus_hunter: [
    'c3d4e5f6-a7b8-9012-cdef-123456789012',
    'd4e5f6a7-b8c9-0123-def0-234567890123',
    'e5f6a7b8-c9d0-1234-ef01-345678901234',
  ],
  top_picks: [
    'b8c9d0e1-f2a3-4567-1234-678901234567',
    'c9d0e1f2-a3b4-5678-2345-789012345678',
    'd0e1f2a3-b4c5-6789-3456-890123456789',
  ],
};

const CONTENT_TYPES = Object.keys(CONTENT_IDS);

function randomItem(arr) {
  return arr[Math.floor(Math.random() * arr.length)];
}
function randomContent() {
  const ct = randomItem(CONTENT_TYPES);
  return { content_type: ct, content_id: randomItem(CONTENT_IDS[ct]) };
}
function authHeaders(token) {
  return { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` };
}

// ---------------------------------------------------------------------------
// gRPC client setup
// ---------------------------------------------------------------------------

const grpcClient = new grpc.Client();
grpcClient.load([PROTO_DIR], 'social/v1/likes.proto');

// ---------------------------------------------------------------------------
// Scenarios — all run simultaneously for ~60s
// ---------------------------------------------------------------------------

export const options = {
  scenarios: {
    // HTTP reads: 2k rps
    http_read: {
      executor: 'constant-arrival-rate',
      exec: 'httpRead',
      rate: 2000,
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 100,
      maxVUs: 200,
      tags: { scenario: 'http_read' },
    },

    // HTTP writes: 200 rps like/unlike
    http_write: {
      executor: 'constant-arrival-rate',
      exec: 'httpWrite',
      rate: 200,
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 30,
      maxVUs: 60,
      tags: { scenario: 'http_write' },
    },

    // HTTP batch: 200 rps
    http_batch: {
      executor: 'constant-arrival-rate',
      exec: 'httpBatch',
      rate: 200,
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 20,
      maxVUs: 40,
      tags: { scenario: 'http_batch' },
    },

    // gRPC reads: 2k rps
    grpc_read: {
      executor: 'constant-arrival-rate',
      exec: 'grpcRead',
      rate: 2000,
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 100,
      maxVUs: 200,
      tags: { scenario: 'grpc_read' },
    },

    // gRPC writes: 200 rps
    grpc_write: {
      executor: 'constant-arrival-rate',
      exec: 'grpcWrite',
      rate: 200,
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 30,
      maxVUs: 60,
      tags: { scenario: 'grpc_write' },
    },

    // gRPC batch: 200 rps
    grpc_batch: {
      executor: 'constant-arrival-rate',
      exec: 'grpcBatch',
      rate: 200,
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 20,
      maxVUs: 40,
      tags: { scenario: 'grpc_batch' },
    },

    // SSE: ramp to 50 connections, hold, drain
    sse: {
      executor: 'ramping-vus',
      exec: 'sseSubscribe',
      startVUs: 0,
      stages: [
        { duration: '10s', target: 50 },
        { duration: '40s', target: 50 },
        { duration: '10s', target: 0 },
      ],
      gracefulRampDown: '10s',
      gracefulStop: '10s',
      tags: { scenario: 'sse' },
    },
  },
  thresholds: {
    'http_req_failed{scenario:http_read}': ['rate<0.01'],
    'http_req_failed{scenario:http_write}': ['rate<0.01'],
    'http_req_failed{scenario:http_batch}': ['rate<0.01'],
  },
};

// ---------------------------------------------------------------------------
// HTTP functions
// ---------------------------------------------------------------------------

export function httpRead() {
  const { content_type, content_id } = randomContent();
  const res = http.get(`${BASE_URL}/v1/likes/${content_type}/${content_id}/count`, {
    tags: { name: 'GET_count' },
  });
  check(res, { 'http read ok': (r) => r.status === 200 || r.status === 429 });
}

export function httpWrite() {
  const token = TOKENS[__VU % TOKENS.length];
  const ct = CONTENT_TYPES[__VU % CONTENT_TYPES.length];
  const cid = CONTENT_IDS[ct][Math.floor(__VU / CONTENT_TYPES.length) % CONTENT_IDS[ct].length];

  if (__ITER % 2 === 0) {
    const res = http.post(`${BASE_URL}/v1/likes`,
      JSON.stringify({ content_type: ct, content_id: cid }),
      { headers: authHeaders(token), tags: { name: 'POST_like' } },
    );
    check(res, { 'http like ok': (r) => r.status === 201 || r.status === 429 });
  } else {
    const res = http.del(`${BASE_URL}/v1/likes/${ct}/${cid}`, null, {
      headers: authHeaders(token), tags: { name: 'DELETE_unlike' },
    });
    check(res, { 'http unlike ok': (r) => r.status === 200 || r.status === 429 });
  }
}

export function httpBatch() {
  const items = [];
  for (let i = 0; i < 20; i++) items.push(randomContent());
  const res = http.post(`${BASE_URL}/v1/likes/batch/counts`,
    JSON.stringify({ items }),
    { headers: { 'Content-Type': 'application/json' }, tags: { name: 'POST_batch_counts' } },
  );
  check(res, { 'http batch ok': (r) => r.status === 200 || r.status === 429 });
}

export function sseSubscribe() {
  const { content_type, content_id } = randomContent();
  http.get(`${BASE_URL}/v1/likes/stream?content_type=${content_type}&content_id=${content_id}`, {
    tags: { name: 'GET_sse_stream' },
    timeout: '35s',
    responseType: 'text',
  });
}

// ---------------------------------------------------------------------------
// gRPC functions
// ---------------------------------------------------------------------------

export function grpcRead() {
  const { content_type, content_id } = randomContent();
  grpcClient.connect(GRPC_HOST, { plaintext: true, timeout: '5s' });
  const res = grpcClient.invoke('social.v1.LikeService/GetCount', {
    content_type, content_id,
  });
  check(res, { 'grpc read ok': (r) => r && r.status === grpc.StatusOK });
  grpcClient.close();
}

export function grpcWrite() {
  const token = TOKENS[__VU % TOKENS.length];
  const ct = CONTENT_TYPES[__VU % CONTENT_TYPES.length];
  const cid = CONTENT_IDS[ct][Math.floor(__VU / CONTENT_TYPES.length) % CONTENT_IDS[ct].length];
  const meta = { authorization: `Bearer ${token}` };

  grpcClient.connect(GRPC_HOST, { plaintext: true, timeout: '5s' });
  if (__ITER % 2 === 0) {
    const res = grpcClient.invoke('social.v1.LikeService/Like', {
      content_type: ct, content_id: cid,
    }, { metadata: meta });
    check(res, { 'grpc like ok': (r) => r && r.status === grpc.StatusOK });
  } else {
    const res = grpcClient.invoke('social.v1.LikeService/Unlike', {
      content_type: ct, content_id: cid,
    }, { metadata: meta });
    check(res, { 'grpc unlike ok': (r) => r && r.status === grpc.StatusOK });
  }
  grpcClient.close();
}

export function grpcBatch() {
  const items = [];
  for (let i = 0; i < 20; i++) items.push(randomContent());
  grpcClient.connect(GRPC_HOST, { plaintext: true, timeout: '5s' });
  const res = grpcClient.invoke('social.v1.LikeService/BatchCounts', { items });
  check(res, { 'grpc batch ok': (r) => r && r.status === grpc.StatusOK });
  grpcClient.close();
}

export default function () {
  httpRead();
}
