import http from 'k6/http';
import grpc from 'k6/net/grpc';
import { check, sleep } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';

// ---------------------------------------------------------------------------
// Unified k6 test — HTTP + gRPC + SSE, all endpoints, all modes
//
// Modes (K6_SCENARIO env var):
//   smoke          Quick 60s all-transport check (CI-friendly)
//   load           Standard 5min load test, moderate RPS
//   stress         Ramping high-RPS sustained stress
//   seed           Populate DB with massive like data
//   comprehensive  Seed + stress all endpoints + race conditions
//
// Usage:
//   make k6-smoke
//   make k6-load
//   make k6-stress
//   make k6-seed
//   make k6-comprehensive
//   k6 run -e K6_SCENARIO=smoke k6/test.js
// ---------------------------------------------------------------------------

const BASE_URL = __ENV.BASE_URL || 'http://localhost:8080';
const GRPC_HOST = __ENV.GRPC_HOST || 'localhost:50051';
const PROTO_DIR = __ENV.PROTO_DIR || '../proto';
const SCENARIO = __ENV.K6_SCENARIO || 'smoke';

// Scale knobs
const NUM_USERS = parseInt(__ENV.NUM_USERS || '100');
const NUM_CONTENT = parseInt(__ENV.NUM_CONTENT || '60');
const STRESS_DURATION = __ENV.STRESS_DURATION || '15m';
const SEED_DURATION = __ENV.SEED_DURATION || '10m';
const MAX_VUS = parseInt(__ENV.MAX_VUS || '1000');

// ---------------------------------------------------------------------------
// Data generation
// ---------------------------------------------------------------------------

const CONTENT_TYPES = ['post', 'bonus_hunter', 'top_picks'];

function generateTokens(n) {
  const tokens = [];
  for (let i = 1; i <= n; i++) tokens.push(`tok_user_${i}`);
  return tokens;
}

// Deterministic UUIDs for content
function generateContentIds(n) {
  const ids = {};
  const perType = Math.ceil(n / CONTENT_TYPES.length);
  CONTENT_TYPES.forEach((ct, typeIdx) => {
    ids[ct] = [];
    for (let i = 0; i < perType; i++) {
      const globalIdx = typeIdx * perType + i + 1;
      const hex = globalIdx.toString(16).padStart(4, '0');
      ids[ct].push(`d0000000-${hex}-4000-8000-000000000000`);
    }
  });
  return ids;
}

const TOKENS = generateTokens(NUM_USERS);
const CONTENT_IDS = generateContentIds(NUM_CONTENT);
const ALL_CONTENT_REFS = CONTENT_TYPES.flatMap((ct) =>
  (CONTENT_IDS[ct] || []).map((cid) => ({ content_type: ct, content_id: cid })),
);

// Pre-built max-size batch payloads (100 items, cycled from available content)
function cycleItems(items, n) {
  const out = [];
  for (let i = 0; i < n; i++) {
    const item = items[i % items.length];
    out.push({ content_type: item.content_type, content_id: item.content_id });
  }
  return out;
}
const FIXED_BATCH_100 = cycleItems(ALL_CONTENT_REFS, 100);

// ---------------------------------------------------------------------------
// Custom metrics
// ---------------------------------------------------------------------------

const seedLikes = new Counter('seed_likes_created');
const errorsByType = new Counter('errors_by_type');
const degradedResponses = new Rate('degraded_responses');
const httpReadLatency = new Trend('http_read_latency', true);
const httpWriteLatency = new Trend('http_write_latency', true);
const httpBatchLatency = new Trend('http_batch_latency', true);
const grpcReadLatency = new Trend('grpc_read_latency', true);
const grpcWriteLatency = new Trend('grpc_write_latency', true);
const grpcBatchLatency = new Trend('grpc_batch_latency', true);

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
  for (let i = 0; i < n; i++) items.push(randomContent());
  return items;
}

// Tell k6 that 429 (rate limited) is an expected status — don't count it as
// a failure in the built-in http_req_failed metric.
http.setResponseCallback(http.expectedStatuses({ min: 200, max: 299 }, 429));

const JSON_HEADERS = { 'Content-Type': 'application/json' };

function authHeaders(token) {
  return { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` };
}

const TIME_WINDOWS_HTTP = ['24h', '7d', '30d', 'all'];
// gRPC TimeWindow enum: DAY=1, WEEK=2, MONTH=3, ALL=4
const TIME_WINDOWS_GRPC = [1, 2, 3, 4];

function isHttpOk(status) {
  return status === 200 || status === 201 || status === 429;
}

// VU allocation helper
function vusFor(targetRps) {
  const pre = Math.max(10, Math.round(targetRps * 0.05));
  const max = Math.max(50, Math.round(targetRps * 0.2));
  return {
    preAllocatedVUs: Math.min(pre, Math.round(MAX_VUS * 0.25)),
    maxVUs: Math.min(max, MAX_VUS),
  };
}

// ---------------------------------------------------------------------------
// gRPC client setup
// ---------------------------------------------------------------------------

const grpcClient = new grpc.Client();
grpcClient.load([PROTO_DIR], 'social/v1/likes.proto');
grpcClient.load([PROTO_DIR], 'social/v1/health.proto');

let __vuConnected = false;

function ensureGrpcConnected() {
  if (__vuConnected) return;
  try {
    grpcClient.connect(GRPC_HOST, { plaintext: true, timeout: '5s' });
    __vuConnected = true;
  } catch (e) {
    __vuConnected = false;
    throw e;
  }
}

function handleGrpcConnError() {
  __vuConnected = false;
}

function isGrpcOk(status) {
  return status === grpc.StatusOK || status === 8; // 8 = ResourceExhausted (rate limited)
}

function grpcAuthParams(token) {
  return { metadata: { authorization: `Bearer ${token}` } };
}

// ═══════════════════════════════════════════════════════════════════════════
// HTTP SCENARIO FUNCTIONS
// ═══════════════════════════════════════════════════════════════════════════

// Mixed HTTP reads: randomly picks count, status, user_likes, leaderboard, filtered_leaderboard
export function httpReadMixed() {
  const roll = Math.random();

  if (roll < 0.30) {
    // GET /v1/likes/{type}/{id}/count
    const { content_type, content_id } = randomContent();
    const res = http.get(`${BASE_URL}/v1/likes/${content_type}/${content_id}/count`, {
      tags: { name: 'GET_count' },
    });
    httpReadLatency.add(res.timings.duration);
    if (!isHttpOk(res.status)) errorsByType.add(1, { type: 'http_count' });
    degradedResponses.add(res.timings.duration > 100);
    check(res, { 'http count ok': (r) => isHttpOk(r.status) });
  } else if (roll < 0.50) {
    // GET /v1/likes/{type}/{id}/status
    const token = randomToken();
    const { content_type, content_id } = randomContent();
    const res = http.get(`${BASE_URL}/v1/likes/${content_type}/${content_id}/status`, {
      headers: authHeaders(token),
      tags: { name: 'GET_status' },
    });
    httpReadLatency.add(res.timings.duration);
    if (!isHttpOk(res.status)) errorsByType.add(1, { type: 'http_status' });
    check(res, { 'http status ok': (r) => isHttpOk(r.status) });
  } else if (roll < 0.70) {
    // GET /v1/likes/user
    const token = randomToken();
    const limit = randomItem([10, 25, 50]);
    let url = `${BASE_URL}/v1/likes/user?limit=${limit}`;
    if (Math.random() < 0.3) url += `&content_type=${randomItem(CONTENT_TYPES)}`;
    const res = http.get(url, {
      headers: authHeaders(token),
      tags: { name: 'GET_user_likes' },
    });
    httpReadLatency.add(res.timings.duration);
    if (!isHttpOk(res.status)) errorsByType.add(1, { type: 'http_user_likes' });
    check(res, { 'http user_likes ok': (r) => isHttpOk(r.status) });
  } else if (roll < 0.85) {
    // GET /v1/likes/top (unfiltered)
    const window = randomItem(TIME_WINDOWS_HTTP);
    const limit = randomItem([10, 25, 50]);
    const res = http.get(`${BASE_URL}/v1/likes/top?window=${window}&limit=${limit}`, {
      tags: { name: 'GET_leaderboard' },
    });
    httpReadLatency.add(res.timings.duration);
    if (!isHttpOk(res.status)) errorsByType.add(1, { type: 'http_leaderboard' });
    check(res, { 'http leaderboard ok': (r) => isHttpOk(r.status) });
  } else {
    // GET /v1/likes/top?content_type=X (filtered)
    const window = randomItem(TIME_WINDOWS_HTTP);
    const ct = randomItem(CONTENT_TYPES);
    const limit = randomItem([10, 25, 50]);
    const res = http.get(
      `${BASE_URL}/v1/likes/top?window=${window}&content_type=${ct}&limit=${limit}`,
      { tags: { name: 'GET_leaderboard_filtered' } },
    );
    httpReadLatency.add(res.timings.duration);
    if (!isHttpOk(res.status)) errorsByType.add(1, { type: 'http_leaderboard_filtered' });
    check(res, { 'http leaderboard_filtered ok': (r) => isHttpOk(r.status) });
  }
}

// HTTP write: alternate like/unlike per VU iteration
export function httpWriteCycle() {
  const token = TOKENS[__VU % TOKENS.length];
  const ctIdx = __VU % CONTENT_TYPES.length;
  const ct = CONTENT_TYPES[ctIdx];
  const ids = CONTENT_IDS[ct];
  const cid = ids[Math.floor(__VU / CONTENT_TYPES.length) % ids.length];

  if (__ITER % 2 === 0) {
    const res = http.post(`${BASE_URL}/v1/likes`,
      JSON.stringify({ content_type: ct, content_id: cid }),
      { headers: authHeaders(token), tags: { name: 'POST_like' } },
    );
    httpWriteLatency.add(res.timings.duration);
    if (res.status !== 201 && res.status !== 429) errorsByType.add(1, { type: 'http_like' });
    check(res, { 'http like ok': (r) => r.status === 201 || r.status === 429 });
  } else {
    const res = http.del(`${BASE_URL}/v1/likes/${ct}/${cid}`, null, {
      headers: authHeaders(token), tags: { name: 'DELETE_unlike' },
    });
    httpWriteLatency.add(res.timings.duration);
    if (res.status !== 200 && res.status !== 429) errorsByType.add(1, { type: 'http_unlike' });
    check(res, { 'http unlike ok': (r) => r.status === 200 || r.status === 429 });
  }
}

// HTTP batch: randomly picks batch_counts (100 items) or batch_statuses (100 items)
export function httpBatchMixed() {
  if (Math.random() < 0.5) {
    // POST /v1/likes/batch/counts
    const res = http.post(`${BASE_URL}/v1/likes/batch/counts`,
      JSON.stringify({ items: FIXED_BATCH_100 }),
      { headers: JSON_HEADERS, tags: { name: 'POST_batch_counts' } },
    );
    httpBatchLatency.add(res.timings.duration);
    if (!isHttpOk(res.status)) errorsByType.add(1, { type: 'http_batch_counts' });
    check(res, { 'http batch_counts ok': (r) => isHttpOk(r.status) });
  } else {
    // POST /v1/likes/batch/statuses
    const token = randomToken();
    const res = http.post(`${BASE_URL}/v1/likes/batch/statuses`,
      JSON.stringify({ items: FIXED_BATCH_100 }),
      { headers: authHeaders(token), tags: { name: 'POST_batch_statuses' } },
    );
    httpBatchLatency.add(res.timings.duration);
    if (!isHttpOk(res.status)) errorsByType.add(1, { type: 'http_batch_statuses' });
    check(res, { 'http batch_statuses ok': (r) => isHttpOk(r.status) });
  }
}

// HTTP write targeting HOT content (race condition detector)
export function httpWriteHot() {
  const token = randomToken();
  // All VUs target the first few content items
  const hot = ALL_CONTENT_REFS[__VU % Math.min(5, ALL_CONTENT_REFS.length)];

  if (__ITER % 2 === 0) {
    const res = http.post(`${BASE_URL}/v1/likes`,
      JSON.stringify({ content_type: hot.content_type, content_id: hot.content_id }),
      { headers: authHeaders(token), tags: { name: 'POST_like_hot' } },
    );
    httpWriteLatency.add(res.timings.duration);
    check(res, { 'http hot_like ok': (r) => r.status === 201 || r.status === 429 });
  } else {
    const res = http.del(
      `${BASE_URL}/v1/likes/${hot.content_type}/${hot.content_id}`, null,
      { headers: authHeaders(token), tags: { name: 'DELETE_unlike_hot' } },
    );
    httpWriteLatency.add(res.timings.duration);
    check(res, { 'http hot_unlike ok': (r) => r.status === 200 || r.status === 429 });
  }
}

// SSE: hold connection for ~35s
export function sseSubscribe() {
  const { content_type, content_id } = randomContent();
  const res = http.get(
    `${BASE_URL}/v1/likes/stream?content_type=${content_type}&content_id=${content_id}`,
    { tags: { name: 'GET_sse_stream' }, timeout: '35s', responseType: 'text' },
  );
  check(res, {
    'sse: received data': (r) => r.body && r.body.includes('event'),
  });
}

// Seed: random user likes random content
export function seedLike() {
  const token = randomToken();
  const { content_type, content_id } = randomContent();
  const res = http.post(`${BASE_URL}/v1/likes`,
    JSON.stringify({ content_type, content_id }),
    { headers: authHeaders(token), tags: { name: 'POST_seed_like' } },
  );
  if (res.status === 201) seedLikes.add(1);
  check(res, { 'seed ok': (r) => r.status === 201 || r.status === 429 });
}

// ═══════════════════════════════════════════════════════════════════════════
// gRPC SCENARIO FUNCTIONS
// ═══════════════════════════════════════════════════════════════════════════

// Mixed gRPC reads: randomly picks GetCount, GetStatus, GetUserLikes, GetLeaderboard
export function grpcReadMixed() {
  try { ensureGrpcConnected(); } catch (_) { return; }

  const roll = Math.random();

  if (roll < 0.30) {
    // GetCount
    const { content_type, content_id } = randomContent();
    const res = grpcClient.invoke('social.v1.LikeService/GetCount',
      { content_type, content_id },
      { tags: { name: 'gRPC_GetCount' } },
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcReadLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc count ok': (r) => isGrpcOk(r.status) });
  } else if (roll < 0.50) {
    // GetStatus
    const token = randomToken();
    const { content_type, content_id } = randomContent();
    const res = grpcClient.invoke('social.v1.LikeService/GetStatus',
      { content_type, content_id },
      Object.assign({ tags: { name: 'gRPC_GetStatus' } }, grpcAuthParams(token)),
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcReadLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc status ok': (r) => isGrpcOk(r.status) });
  } else if (roll < 0.70) {
    // GetUserLikes
    const token = randomToken();
    const limit = randomItem([10, 25, 50]);
    const req = { pagination: { limit } };
    if (Math.random() < 0.3) req.content_type = randomItem(CONTENT_TYPES);
    const res = grpcClient.invoke('social.v1.LikeService/GetUserLikes', req,
      Object.assign({ tags: { name: 'gRPC_GetUserLikes' } }, grpcAuthParams(token)),
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcReadLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc user_likes ok': (r) => isGrpcOk(r.status) });
  } else if (roll < 0.85) {
    // GetLeaderboard (unfiltered)
    const window = randomItem(TIME_WINDOWS_GRPC);
    const limit = randomItem([10, 25, 50]);
    const res = grpcClient.invoke('social.v1.LikeService/GetLeaderboard',
      { window, limit },
      { tags: { name: 'gRPC_GetLeaderboard' } },
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcReadLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc leaderboard ok': (r) => isGrpcOk(r.status) });
  } else {
    // GetLeaderboard (filtered)
    const window = randomItem(TIME_WINDOWS_GRPC);
    const ct = randomItem(CONTENT_TYPES);
    const limit = randomItem([10, 25, 50]);
    const res = grpcClient.invoke('social.v1.LikeService/GetLeaderboard',
      { window, content_type: ct, limit },
      { tags: { name: 'gRPC_GetLeaderboard_filtered' } },
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcReadLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc leaderboard_filtered ok': (r) => isGrpcOk(r.status) });
  }
}

// gRPC write: alternate Like/Unlike per VU iteration
export function grpcWriteCycle() {
  try { ensureGrpcConnected(); } catch (_) { return; }

  const token = TOKENS[__VU % TOKENS.length];
  const ctIdx = __VU % CONTENT_TYPES.length;
  const ct = CONTENT_TYPES[ctIdx];
  const ids = CONTENT_IDS[ct];
  const cid = ids[Math.floor(__VU / CONTENT_TYPES.length) % ids.length];
  const params = grpcAuthParams(token);

  if (__ITER % 2 === 0) {
    const res = grpcClient.invoke('social.v1.LikeService/Like',
      { content_type: ct, content_id: cid },
      Object.assign({ tags: { name: 'gRPC_Like' } }, params),
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcWriteLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc like ok': (r) => isGrpcOk(r.status) });
  } else {
    const res = grpcClient.invoke('social.v1.LikeService/Unlike',
      { content_type: ct, content_id: cid },
      Object.assign({ tags: { name: 'gRPC_Unlike' } }, params),
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcWriteLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc unlike ok': (r) => isGrpcOk(r.status) });
  }
}

// gRPC batch: randomly picks BatchCounts or BatchStatuses with 100 items
export function grpcBatchMixed() {
  try { ensureGrpcConnected(); } catch (_) { return; }

  if (Math.random() < 0.5) {
    const res = grpcClient.invoke('social.v1.LikeService/BatchCounts',
      { items: FIXED_BATCH_100 },
      { tags: { name: 'gRPC_BatchCounts' } },
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcBatchLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc batch_counts ok': (r) => isGrpcOk(r.status) });
  } else {
    const token = randomToken();
    const res = grpcClient.invoke('social.v1.LikeService/BatchStatuses',
      { items: FIXED_BATCH_100 },
      Object.assign({ tags: { name: 'gRPC_BatchStatuses' } }, grpcAuthParams(token)),
    );
    if (!res) { handleGrpcConnError(); return; }
    grpcBatchLatency.add(res.timings && res.timings.duration || 0);
    check(res, { 'grpc batch_statuses ok': (r) => isGrpcOk(r.status) });
  }
}

// gRPC health check
export function grpcHealthCheck() {
  try { ensureGrpcConnected(); } catch (_) { return; }
  const res = grpcClient.invoke('social.v1.Health/Check', { service: '' },
    { tags: { name: 'gRPC_Health' } },
  );
  if (!res) { handleGrpcConnError(); return; }
  check(res, { 'grpc health ok': (r) => r.status === grpc.StatusOK });
}

// ═══════════════════════════════════════════════════════════════════════════
// SCENARIOS
// ═══════════════════════════════════════════════════════════════════════════

// --- smoke: Quick 60s all-transport check ---

const SMOKE_SCENARIOS = {
  http_reads: {
    executor: 'constant-arrival-rate',
    exec: 'httpReadMixed',
    rate: 500, timeUnit: '1s', duration: '60s',
    preAllocatedVUs: 30, maxVUs: 60,
    gracefulStop: '10s',
    tags: { scenario: 'http_reads' },
  },
  http_writes: {
    executor: 'constant-arrival-rate',
    exec: 'httpWriteCycle',
    rate: 50, timeUnit: '1s', duration: '60s',
    preAllocatedVUs: 10, maxVUs: 20,
    gracefulStop: '10s',
    tags: { scenario: 'http_writes' },
  },
  http_batch: {
    executor: 'constant-arrival-rate',
    exec: 'httpBatchMixed',
    rate: 50, timeUnit: '1s', duration: '60s',
    preAllocatedVUs: 10, maxVUs: 20,
    gracefulStop: '10s',
    tags: { scenario: 'http_batch' },
  },
  grpc_reads: {
    executor: 'constant-arrival-rate',
    exec: 'grpcReadMixed',
    rate: 500, timeUnit: '1s', duration: '60s',
    preAllocatedVUs: 20, maxVUs: 40,
    gracefulStop: '10s',
    tags: { scenario: 'grpc_reads' },
  },
  grpc_writes: {
    executor: 'constant-arrival-rate',
    exec: 'grpcWriteCycle',
    rate: 50, timeUnit: '1s', duration: '60s',
    preAllocatedVUs: 10, maxVUs: 20,
    gracefulStop: '10s',
    tags: { scenario: 'grpc_writes' },
  },
  grpc_batch: {
    executor: 'constant-arrival-rate',
    exec: 'grpcBatchMixed',
    rate: 50, timeUnit: '1s', duration: '60s',
    preAllocatedVUs: 10, maxVUs: 20,
    gracefulStop: '10s',
    tags: { scenario: 'grpc_batch' },
  },
  sse: {
    executor: 'ramping-vus',
    exec: 'sseSubscribe',
    startVUs: 0,
    stages: [
      { duration: '10s', target: 20 },
      { duration: '40s', target: 20 },
      { duration: '10s', target: 0 },
    ],
    gracefulRampDown: '10s', gracefulStop: '10s',
    tags: { scenario: 'sse' },
  },
};

// --- load: Standard 5min parallel load ---

const LOAD_SCENARIOS = {
  http_reads: {
    executor: 'constant-arrival-rate',
    exec: 'httpReadMixed',
    rate: 5000, timeUnit: '1s', duration: '5m',
    preAllocatedVUs: 100, maxVUs: 300,
    gracefulStop: '10s',
    tags: { scenario: 'http_reads' },
  },
  http_writes: {
    executor: 'constant-arrival-rate',
    exec: 'httpWriteCycle',
    rate: 300, timeUnit: '1s', duration: '5m',
    preAllocatedVUs: 30, maxVUs: 60,
    gracefulStop: '10s',
    tags: { scenario: 'http_writes' },
  },
  http_batch: {
    executor: 'constant-arrival-rate',
    exec: 'httpBatchMixed',
    rate: 200, timeUnit: '1s', duration: '5m',
    preAllocatedVUs: 20, maxVUs: 50,
    gracefulStop: '10s',
    tags: { scenario: 'http_batch' },
  },
  grpc_reads: {
    executor: 'constant-arrival-rate',
    exec: 'grpcReadMixed',
    rate: 5000, timeUnit: '1s', duration: '5m',
    preAllocatedVUs: 50, maxVUs: 100,
    gracefulStop: '10s',
    tags: { scenario: 'grpc_reads' },
  },
  grpc_writes: {
    executor: 'constant-arrival-rate',
    exec: 'grpcWriteCycle',
    rate: 200, timeUnit: '1s', duration: '5m',
    preAllocatedVUs: 15, maxVUs: 30,
    gracefulStop: '10s',
    tags: { scenario: 'grpc_writes' },
  },
  grpc_batch: {
    executor: 'constant-arrival-rate',
    exec: 'grpcBatchMixed',
    rate: 200, timeUnit: '1s', duration: '5m',
    preAllocatedVUs: 15, maxVUs: 30,
    gracefulStop: '10s',
    tags: { scenario: 'grpc_batch' },
  },
  sse: {
    executor: 'ramping-vus',
    exec: 'sseSubscribe',
    startVUs: 0,
    stages: [
      { duration: '30s', target: 200 },
      { duration: '4m', target: 200 },
      { duration: '30s', target: 0 },
    ],
    gracefulRampDown: '15s', gracefulStop: '15s',
    tags: { scenario: 'sse' },
  },
};

// --- stress: Ramping high-RPS sustained stress ---

const STRESS_SCENARIOS = {
  http_reads: {
    executor: 'ramping-arrival-rate',
    exec: 'httpReadMixed',
    startRate: 500, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 2000 },
      { duration: '2m', target: 5000 },
      { duration: '2m', target: 8000 },
      { duration: STRESS_DURATION, target: 8000 },
      { duration: '2m', target: 0 },
    ],
    ...vusFor(8000),
    gracefulStop: '30s',
    tags: { scenario: 'http_reads' },
  },
  http_writes: {
    executor: 'ramping-arrival-rate',
    exec: 'httpWriteCycle',
    startRate: 50, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 200 },
      { duration: '2m', target: 500 },
      { duration: '2m', target: 1000 },
      { duration: STRESS_DURATION, target: 1000 },
      { duration: '2m', target: 0 },
    ],
    ...vusFor(1000),
    gracefulStop: '30s',
    tags: { scenario: 'http_writes' },
  },
  http_batch: {
    executor: 'ramping-arrival-rate',
    exec: 'httpBatchMixed',
    startRate: 20, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 100 },
      { duration: '2m', target: 300 },
      { duration: '2m', target: 500 },
      { duration: STRESS_DURATION, target: 500 },
      { duration: '2m', target: 0 },
    ],
    ...vusFor(500),
    gracefulStop: '30s',
    tags: { scenario: 'http_batch' },
  },
  grpc_reads: {
    executor: 'ramping-arrival-rate',
    exec: 'grpcReadMixed',
    startRate: 500, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 2000 },
      { duration: '2m', target: 4000 },
      { duration: '2m', target: 5000 },
      { duration: STRESS_DURATION, target: 5000 },
      { duration: '2m', target: 0 },
    ],
    ...vusFor(5000),
    gracefulStop: '30s',
    tags: { scenario: 'grpc_reads' },
  },
  grpc_writes: {
    executor: 'ramping-arrival-rate',
    exec: 'grpcWriteCycle',
    startRate: 50, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 200 },
      { duration: '2m', target: 400 },
      { duration: '2m', target: 500 },
      { duration: STRESS_DURATION, target: 500 },
      { duration: '2m', target: 0 },
    ],
    ...vusFor(500),
    gracefulStop: '30s',
    tags: { scenario: 'grpc_writes' },
  },
  grpc_batch: {
    executor: 'ramping-arrival-rate',
    exec: 'grpcBatchMixed',
    startRate: 20, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 100 },
      { duration: '2m', target: 200 },
      { duration: '2m', target: 300 },
      { duration: STRESS_DURATION, target: 300 },
      { duration: '2m', target: 0 },
    ],
    ...vusFor(300),
    gracefulStop: '30s',
    tags: { scenario: 'grpc_batch' },
  },
  sse: {
    executor: 'ramping-vus',
    exec: 'sseSubscribe',
    startVUs: 0,
    stages: [
      { duration: '2m', target: 200 },
      { duration: '2m', target: 500 },
      { duration: '2m', target: 1000 },
      { duration: STRESS_DURATION, target: 1000 },
      { duration: '2m', target: 0 },
    ],
    gracefulRampDown: '15s', gracefulStop: '40s',
    tags: { scenario: 'sse' },
  },
};

// --- seed: Populate DB with massive like data ---

const SEED_SCENARIOS = {
  seed_likes: {
    executor: 'constant-arrival-rate',
    exec: 'seedLike',
    rate: 5000, timeUnit: '1s', duration: SEED_DURATION,
    preAllocatedVUs: 200, maxVUs: 1000,
    gracefulStop: '30s',
    tags: { scenario: 'seed_likes' },
  },
};

// --- comprehensive: seed → read stress → write pressure (all overlapping) ---

const COMPREHENSIVE_SCENARIOS = {
  // Phase 1: Seed
  seed_likes: {
    ...SEED_SCENARIOS.seed_likes,
  },

  // Phase 2: All reads (starts after seed finishes + 1m warmup)
  http_reads: {
    executor: 'ramping-arrival-rate',
    exec: 'httpReadMixed',
    startRate: 200, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 2000 },
      { duration: STRESS_DURATION, target: 2000 },
      { duration: '1m', target: 0 },
    ],
    ...vusFor(2000),
    gracefulStop: '15s',
    startTime: `${parseInt(SEED_DURATION) + 1}m`,
    tags: { scenario: 'http_reads' },
  },
  grpc_reads: {
    executor: 'ramping-arrival-rate',
    exec: 'grpcReadMixed',
    startRate: 200, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 2000 },
      { duration: STRESS_DURATION, target: 2000 },
      { duration: '1m', target: 0 },
    ],
    ...vusFor(2000),
    gracefulStop: '15s',
    startTime: `${parseInt(SEED_DURATION) + 1}m`,
    tags: { scenario: 'grpc_reads' },
  },
  http_batch: {
    executor: 'ramping-arrival-rate',
    exec: 'httpBatchMixed',
    startRate: 50, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 300 },
      { duration: STRESS_DURATION, target: 300 },
      { duration: '1m', target: 0 },
    ],
    ...vusFor(300),
    gracefulStop: '15s',
    startTime: `${parseInt(SEED_DURATION) + 1}m`,
    tags: { scenario: 'http_batch' },
  },
  grpc_batch: {
    executor: 'ramping-arrival-rate',
    exec: 'grpcBatchMixed',
    startRate: 50, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 300 },
      { duration: STRESS_DURATION, target: 300 },
      { duration: '1m', target: 0 },
    ],
    ...vusFor(300),
    gracefulStop: '15s',
    startTime: `${parseInt(SEED_DURATION) + 1}m`,
    tags: { scenario: 'grpc_batch' },
  },

  // Phase 3: Write pressure (starts 2m after reads, overlapping)
  http_writes: {
    executor: 'ramping-arrival-rate',
    exec: 'httpWriteCycle',
    startRate: 100, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 500 },
      { duration: STRESS_DURATION, target: 500 },
      { duration: '1m', target: 0 },
    ],
    ...vusFor(500),
    gracefulStop: '15s',
    startTime: `${parseInt(SEED_DURATION) + 3}m`,
    tags: { scenario: 'http_writes' },
  },
  grpc_writes: {
    executor: 'ramping-arrival-rate',
    exec: 'grpcWriteCycle',
    startRate: 100, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 500 },
      { duration: STRESS_DURATION, target: 500 },
      { duration: '1m', target: 0 },
    ],
    ...vusFor(500),
    gracefulStop: '15s',
    startTime: `${parseInt(SEED_DURATION) + 3}m`,
    tags: { scenario: 'grpc_writes' },
  },
  // Hot content: race condition detector
  write_hot: {
    executor: 'ramping-arrival-rate',
    exec: 'httpWriteHot',
    startRate: 50, timeUnit: '1s',
    stages: [
      { duration: '2m', target: 300 },
      { duration: STRESS_DURATION, target: 300 },
      { duration: '1m', target: 0 },
    ],
    ...vusFor(300),
    gracefulStop: '15s',
    startTime: `${parseInt(SEED_DURATION) + 3}m`,
    tags: { scenario: 'write_hot' },
  },
  sse: {
    executor: 'ramping-vus',
    exec: 'sseSubscribe',
    startVUs: 0,
    stages: [
      { duration: '1m', target: 100 },
      { duration: STRESS_DURATION, target: 100 },
      { duration: '1m', target: 0 },
    ],
    gracefulRampDown: '15s', gracefulStop: '15s',
    startTime: `${parseInt(SEED_DURATION) + 1}m`,
    tags: { scenario: 'sse' },
  },
};

// ---------------------------------------------------------------------------
// Scenario selector + thresholds
// ---------------------------------------------------------------------------

function buildScenarios() {
  switch (SCENARIO) {
    case 'smoke': return SMOKE_SCENARIOS;
    case 'load': return LOAD_SCENARIOS;
    case 'stress': return STRESS_SCENARIOS;
    case 'seed': return SEED_SCENARIOS;
    case 'comprehensive': return COMPREHENSIVE_SCENARIOS;
    default:
      console.error(`Unknown scenario: ${SCENARIO}. Available: smoke, load, stress, seed, comprehensive`);
      return SMOKE_SCENARIOS;
  }
}

function buildThresholds() {
  const base = {
    // HTTP
    'http_req_failed{scenario:http_reads}': ['rate<0.05'],
    'http_req_failed{scenario:http_writes}': ['rate<0.05'],
    'http_req_failed{scenario:http_batch}': ['rate<0.05'],
    // gRPC (grpc_req_duration is the built-in k6 metric for gRPC)
    'grpc_req_duration{scenario:grpc_reads}': ['p(95)<100'],
    'grpc_req_duration{scenario:grpc_writes}': ['p(95)<300'],
    'grpc_req_duration{scenario:grpc_batch}': ['p(95)<300'],
  };

  switch (SCENARIO) {
    case 'smoke':
      return {
        ...base,
        'http_req_duration{scenario:http_reads}': ['p(99)<50'],
        'http_req_duration{scenario:http_writes}': ['p(99)<100'],
        'http_req_duration{scenario:http_batch}': ['p(99)<100'],
      };
    case 'load':
      return {
        ...base,
        'http_req_duration{scenario:http_reads}': ['p(99)<20'],
        'http_req_duration{scenario:http_writes}': ['p(99)<100'],
        'http_req_duration{scenario:http_batch}': ['p(99)<100'],
        'http_req_failed{scenario:http_reads}': ['rate<0.01'],
        'http_req_failed{scenario:http_writes}': ['rate<0.01'],
        'http_req_failed{scenario:http_batch}': ['rate<0.01'],
      };
    case 'stress':
      return {
        'http_req_duration{scenario:http_reads}': ['p(95)<100', 'p(99)<500'],
        'http_req_duration{scenario:http_writes}': ['p(95)<500', 'p(99)<2000'],
        'http_req_duration{scenario:http_batch}': ['p(95)<300', 'p(99)<1000'],
        'http_req_failed{scenario:http_reads}': ['rate<0.05'],
        'http_req_failed{scenario:http_writes}': ['rate<0.10'],
        'http_req_failed{scenario:http_batch}': ['rate<0.05'],
      };
    case 'seed':
      return {
        'http_req_failed{scenario:seed_likes}': ['rate<0.10'],
      };
    case 'comprehensive':
      return {
        'http_req_failed{scenario:seed_likes}': ['rate<0.10'],
        'http_req_failed{scenario:http_reads}': ['rate<0.05'],
        'http_req_failed{scenario:http_writes}': ['rate<0.10'],
        'http_req_failed{scenario:http_batch}': ['rate<0.05'],
        'http_req_failed{scenario:write_hot}': ['rate<0.10'],
      };
    default:
      return base;
  }
}

export const options = {
  scenarios: buildScenarios(),
  thresholds: buildThresholds(),
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(90)', 'p(95)', 'p(99)', 'count'],
  noConnectionReuse: false,
  batch: 20,
  batchPerHost: 20,
  dns: { ttl: '5m', select: 'roundRobin' },
};

// Default export for ad-hoc usage: k6 run --vus 5 --duration 10s k6/test.js
export default function () {
  httpReadMixed();
}
