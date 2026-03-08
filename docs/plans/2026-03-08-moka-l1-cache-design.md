# Moka L1 In-Process Cache Design

## Problem

At 10k RPS, hot keys in Redis receive thousands of redundant GET calls per second.
Redis handles ~1ms per call, but connection pool contention and network round-trips
add up, increasing tail latency and wasting resources.

## Solution

Add moka (in-process async cache) as an L1 layer inside `CacheManager`, transparent
to all callers. Short TTL (200ms default) balances freshness with load reduction.

## Architecture

```
Request → CacheManager.get() → moka L1 (200ms) → Redis L2 (300s+) → DB
                                  ↑ hit                ↑ hit = populate L1
```

### Operations

| Method | L1 Behavior |
|---|---|
| `get()` | Check L1 → Redis on miss → populate L1 |
| `set()` | Write to L1 + Redis |
| `del()` | Evict from L1 + Redis |
| `mget()` | Check L1 per-key → Redis MGET for misses → populate L1 |
| `set_many()` | Write to L1 + Redis pipeline |
| `zrevrange_with_scores()` | Check L1 (key includes range) → Redis → populate L1 |
| `replace_sorted_set()` | Invalidate all L1 zset entries for key prefix → Redis |
| `invoke_script()` | Bypasses L1 (rate limiting needs real-time accuracy) |

### What benefits

| Path | Why |
|---|---|
| Like counts (`lc:`) | Hot content, 1000s reads/sec same key |
| Token validation (`tok:`) | Same user, many concurrent requests |
| Content validation (`cv:`) | Same content validated by many users |
| Leaderboard (`lb:`) | 3-4 keys, extremely hot |

### What skips L1

- Rate limiting (`invoke_script`) — needs real-time accuracy
- Pub/Sub (`publish`) — fire-and-forget, no caching

## Config

| Env Var | Default | Description |
|---|---|---|
| `LOCAL_CACHE_TTL_MS` | 200 | L1 entry time-to-live in milliseconds |
| `LOCAL_CACHE_MAX_CAPACITY` | 10,000 | Max entries in string L1 cache (zsets = 1/10th) |

## Impact

Hot key at 5000 reads/sec with 200ms L1 TTL: **5 Redis calls/sec** (99.9% reduction).

## Metrics

- `social_api_cache_operations_total{operation="get", result="l1_hit"}` — L1 string hits
- `social_api_cache_operations_total{operation="mget", result="l1_hit"}` — L1 batch hits
- `social_api_cache_operations_total{operation="zrevrange", result="l1_hit"}` — L1 zset hits
