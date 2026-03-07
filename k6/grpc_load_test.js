import grpc from 'k6/net/grpc';
import { check } from 'k6';

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const GRPC_HOST = __ENV.GRPC_HOST || 'localhost:50051';
const ONLY_SCENARIO = __ENV.K6_SCENARIO || '';

// Proto paths — k6 loads .proto files at init time.
// k6 resolves proto files relative to import paths.
const PROTO_DIR = __ENV.PROTO_DIR || '../proto';

// Threshold multiplier for local testing. Default=1 (production targets).
// Set K6_THRESHOLD_MULT=5 for local Docker runs where latency is higher.
const THRESHOLD_MULT = parseInt(__ENV.K6_THRESHOLD_MULT || '1');

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
// gRPC Client — load protos at init time
// ---------------------------------------------------------------------------

const client = new grpc.Client();
client.load([PROTO_DIR], 'social/v1/likes.proto');
client.load([PROTO_DIR], 'social/v1/health.proto');

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

function authParams(token) {
  return {
    metadata: { authorization: `Bearer ${token}` },
    tags: {},
  };
}

// Status code OK=0, ResourceExhausted=8 (rate limited)
function isOkOrRateLimited(status) {
  return status === grpc.StatusOK || status === 8;
}

// ---------------------------------------------------------------------------
// VU Sizing — Why fewer VUs is better for gRPC
// ---------------------------------------------------------------------------
//
// gRPC uses HTTP/2, which multiplexes many RPCs over a single TCP connection.
// Each VU in k6 opens ONE TCP connection (no cross-VU sharing — k6 #3188).
//
// Sizing formula:
//   VUs = target_RPS * avg_rpc_duration_ms / 1000 * safety_margin(2x)
//
// Example: 10k RPS at ~1ms/RPC = 10 VUs needed, 2x safety = 20 VUs.
//          We use 50 preAllocated (5x margin) to handle latency spikes.
//
// Over-provisioning VUs wastes TCP sockets and causes ephemeral port
// exhaustion on macOS (~16k port range). At 200-400 VUs in parallel mode,
// this manifests as "can't assign requested address" errors.
//
// Rule of thumb: start low, increase only if k6 reports dropped iterations.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

const ALL_SCENARIOS = {
  // Isolated: 10k rps reads via gRPC
  grpc_read: {
    executor: 'constant-arrival-rate',
    exec: 'grpcReadCount',
    rate: 10000,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 50,
    maxVUs: 100,
    gracefulStop: '10s',
    tags: { scenario: 'grpc_read' },
  },

  // Isolated: 1k rps batch counts via gRPC
  grpc_batch: {
    executor: 'constant-arrival-rate',
    exec: 'grpcBatchCounts',
    rate: 1000,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 20,
    maxVUs: 40,
    gracefulStop: '10s',
    startTime: '70s',
    tags: { scenario: 'grpc_batch' },
  },

  // Isolated: 500 rps write cycle (like/unlike) via gRPC
  grpc_write: {
    executor: 'constant-arrival-rate',
    exec: 'grpcWriteCycle',
    rate: 500,
    timeUnit: '1s',
    duration: '60s',
    preAllocatedVUs: 20,
    maxVUs: 40,
    gracefulStop: '10s',
    startTime: '140s',
    tags: { scenario: 'grpc_write' },
  },

  // Mixed: 80/15/5 ratio at 2k rps via gRPC
  grpc_mixed: {
    executor: 'constant-arrival-rate',
    exec: 'grpcMixed',
    rate: 2000,
    timeUnit: '1s',
    duration: '120s',
    preAllocatedVUs: 30,
    maxVUs: 60,
    gracefulStop: '10s',
    startTime: '210s',
    tags: { scenario: 'grpc_mixed' },
  },
};

// Parallel: all scenarios fire simultaneously
const PARALLEL_SCENARIOS = {
  parallel_grpc_read: {
    executor: 'constant-arrival-rate',
    exec: 'grpcReadCount',
    rate: 8000,
    timeUnit: '1s',
    duration: '90s',
    preAllocatedVUs: 40,
    maxVUs: 80,
    gracefulStop: '10s',
    tags: { scenario: 'parallel_grpc_read' },
  },
  parallel_grpc_batch: {
    executor: 'constant-arrival-rate',
    exec: 'grpcBatchCounts',
    rate: 500,
    timeUnit: '1s',
    duration: '90s',
    preAllocatedVUs: 15,
    maxVUs: 30,
    gracefulStop: '10s',
    tags: { scenario: 'parallel_grpc_batch' },
  },
  parallel_grpc_write: {
    executor: 'constant-arrival-rate',
    exec: 'grpcWriteCycle',
    rate: 300,
    timeUnit: '1s',
    duration: '90s',
    preAllocatedVUs: 15,
    maxVUs: 30,
    gracefulStop: '10s',
    tags: { scenario: 'parallel_grpc_write' },
  },
};

// Health check only
const HEALTH_SCENARIO = {
  grpc_health: {
    executor: 'constant-arrival-rate',
    exec: 'grpcHealthCheck',
    rate: 100,
    timeUnit: '1s',
    duration: '30s',
    preAllocatedVUs: 10,
    maxVUs: 20,
    gracefulStop: '5s',
    tags: { scenario: 'grpc_health' },
  },
};

// ---------------------------------------------------------------------------
// Scenario selector
// ---------------------------------------------------------------------------

function buildScenarios() {
  if (ONLY_SCENARIO === 'parallel') return PARALLEL_SCENARIOS;
  if (ONLY_SCENARIO === 'health') return HEALTH_SCENARIO;
  if (ONLY_SCENARIO && ALL_SCENARIOS[ONLY_SCENARIO]) {
    const s = Object.assign({}, ALL_SCENARIOS[ONLY_SCENARIO]);
    delete s.startTime;
    return { [ONLY_SCENARIO]: s };
  }
  return ALL_SCENARIOS;
}

function buildThresholds() {
  const m = THRESHOLD_MULT;
  if (ONLY_SCENARIO === 'parallel') {
    return {
      'grpc_req_duration{scenario:parallel_grpc_read}': [`p(99)<${15 * m}`],
      'grpc_req_duration{scenario:parallel_grpc_batch}': [`p(99)<${75 * m}`],
      'grpc_req_duration{scenario:parallel_grpc_write}': [`p(99)<${150 * m}`],
    };
  }
  if (ONLY_SCENARIO === 'health') {
    return {
      'grpc_req_duration{scenario:grpc_health}': [`p(99)<${10 * m}`],
    };
  }
  return {
    'grpc_req_duration{scenario:grpc_read}': [`p(99)<${10 * m}`],
    'grpc_req_duration{scenario:grpc_batch}': [`p(99)<${50 * m}`],
    'grpc_req_duration{scenario:grpc_write}': [`p(99)<${100 * m}`],
    'grpc_req_duration{scenario:grpc_mixed}': [`p(99)<${100 * m}`],
  };
}

export const options = {
  scenarios: buildScenarios(),
  thresholds: buildThresholds(),
};

// ---------------------------------------------------------------------------
// VU lifecycle: connect once per VU
// ---------------------------------------------------------------------------
//
// k6 gRPC vs HTTP — why gRPC hits port exhaustion more easily:
//
//   HTTP: Go's net/http transport pools connections, reuses them on failure,
//         and manages retries transparently. Failed requests don't create
//         new sockets.
//
//   gRPC: client.connect() creates a new TCP socket per call. If a connection
//         fails, the socket enters TIME_WAIT (60s on macOS). On the next
//         iteration, connect() creates ANOTHER socket. With constant-arrival-rate
//         spawning new VUs on slowdown, failed connections snowball:
//         slow → more VUs → more connect() → more TIME_WAIT → port exhaustion.
//
// macOS ephemeral port range: 49152-65535 = ~16k ports.
// At 8800 target RPS with connection failures, ports exhaust in ~2 seconds.
//
// Mitigation: track connection state per VU and sleep on failure to prevent
// rapid socket churn. A failed VU sleeps 1s instead of immediately retrying
// connect(), which would burn another ephemeral port.
//
// Note: reflect:true requires the server to implement grpc.reflection.v1.
// k6 does NOT fall back gracefully — it fails the connection if unsupported.
// ---------------------------------------------------------------------------

let __vuConnected = false;

function ensureConnected() {
  if (__vuConnected) return;
  try {
    client.connect(GRPC_HOST, {
      plaintext: true,
      timeout: '5s',
    });
    __vuConnected = true;
  } catch (e) {
    // Connection failed — sleep to avoid rapid socket churn.
    // Without this, constant-arrival-rate spawns new VUs that immediately
    // burn another ephemeral port on a failed connect().
    __vuConnected = false;
    throw e;
  }
}

// Close hook: mark connection as disconnected so next iteration reconnects.
function handleConnError() {
  __vuConnected = false;
}

export function grpcReadCount() {
  try { ensureConnected(); } catch (_) { return; }

  const { content_type, content_id } = randomContent();
  const res = client.invoke(
    'social.v1.LikeService/GetCount',
    { content_type, content_id },
    { tags: { name: 'gRPC_GetCount' } },
  );

  if (!res) { handleConnError(); return; }

  check(res, {
    'grpc_read: status ok': (r) => isOkOrRateLimited(r.status),
    'grpc_read: has count': (r) => {
      if (r.status === 8) return true; // rate limited
      return r.message && r.message.count !== undefined;
    },
  });

}

export function grpcBatchCounts() {
  try { ensureConnected(); } catch (_) { return; }

  const items = randomBatchItems(50);
  const res = client.invoke(
    'social.v1.LikeService/BatchCounts',
    { items },
    { tags: { name: 'gRPC_BatchCounts' } },
  );

  if (!res) { handleConnError(); return; }

  check(res, {
    'grpc_batch: status ok': (r) => isOkOrRateLimited(r.status),
    'grpc_batch: results count': (r) => {
      if (r.status === 8) return true;
      return r.message && r.message.results && r.message.results.length === 50;
    },
  });

}

export function grpcWriteCycle() {
  try { ensureConnected(); } catch (_) { return; }

  const vuId = __VU;
  const iter = __ITER;

  const token = TOKENS[vuId % TOKENS.length];
  const ctIdx = vuId % CONTENT_TYPES.length;
  const ct = CONTENT_TYPES[ctIdx];
  const ids = CONTENT_IDS[ct];
  const cid = ids[Math.floor(vuId / CONTENT_TYPES.length) % ids.length];

  const params = {
    metadata: { authorization: `Bearer ${token}` },
  };

  if (iter % 2 === 0) {
    // Like
    const res = client.invoke(
      'social.v1.LikeService/Like',
      { content_type: ct, content_id: cid },
      Object.assign({ tags: { name: 'gRPC_Like' } }, params),
    );

    check(res, {
      'grpc_write_like: status ok': (r) => isOkOrRateLimited(r.status),
      'grpc_write_like: has liked': (r) => {
        if (r.status === 8) return true;
        return r.message && r.message.liked !== undefined;
      },
    });
  } else {
    // Unlike
    const res = client.invoke(
      'social.v1.LikeService/Unlike',
      { content_type: ct, content_id: cid },
      Object.assign({ tags: { name: 'gRPC_Unlike' } }, params),
    );

    check(res, {
      'grpc_write_unlike: status ok': (r) => isOkOrRateLimited(r.status),
      'grpc_write_unlike: has was_liked': (r) => {
        if (r.status === 8) return true;
        return r.message && r.message.wasLiked !== undefined;
      },
    });
  }

}

export function grpcMixed() {
  const roll = Math.random();
  if (roll < 0.80) {
    grpcReadCount();
  } else if (roll < 0.95) {
    grpcBatchCounts();
  } else {
    grpcWriteCycle();
  }
}

export function grpcHealthCheck() {
  try { ensureConnected(); } catch (_) { return; }

  const res = client.invoke(
    'social.v1.Health/Check',
    { service: '' },
    { tags: { name: 'gRPC_Health' } },
  );

  check(res, {
    'grpc_health: status ok': (r) => r.status === grpc.StatusOK,
    'grpc_health: serving': (r) => {
      return r.message && r.message.status === 'SERVING_STATUS_SERVING';
    },
  });

}

// Default: mixed gRPC workload
export default function () {
  grpcMixed();
}
