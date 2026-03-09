//! gRPC rate limiting helper.
//!
//! Unlike the HTTP middleware (which uses axum's `from_fn_with_state`), gRPC
//! rate limiting is an async operation that requires Redis access. It cannot
//! be a tonic interceptor (which is sync). Instead, gRPC service methods call
//! this helper at the top of each handler.

use crate::cache::CacheManager;
use crate::middleware::rate_limit::{check_rate_limit_inner, fnv1a_hash};

/// Check whether the caller has exceeded their rate limit.
///
/// - `identifier`: the token (for writes) or IP (for reads) to rate-limit on.
/// - `is_write`: selects the write or read rate-limit bucket.
/// - `write_limit` / `read_limit`: requests per minute.
///
/// Returns `Ok(())` if allowed, or `Err(tonic::Status::resource_exhausted(...))`
/// with a retry-after message if the limit is exceeded.
///
/// On Redis failure the request is **allowed** (fail-open), consistent with
/// the HTTP rate limiter's behavior.
#[allow(clippy::result_large_err)]
pub async fn check_grpc_rate_limit(
    cache: &CacheManager,
    identifier: &str,
    is_write: bool,
    write_limit: u64,
    read_limit: u64,
) -> Result<(), tonic::Status> {
    let (prefix, limit) = if is_write {
        ("rl:user:write", write_limit)
    } else {
        ("rl:ip:read", read_limit)
    };
    let key = format!("{prefix}:{}", fnv1a_hash(identifier));
    let result = check_rate_limit_inner(cache, &key, limit, 60).await;

    if !result.allowed {
        return Err(tonic::Status::resource_exhausted(format!(
            "Rate limit exceeded. Retry after {}s",
            result.reset_secs
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_key_uses_write_prefix() {
        // Verify the key construction logic by checking the hash-based key format.
        // The full async path requires Redis; here we test the sync prefix selection.
        let prefix = if true { "rl:user:write" } else { "rl:ip:read" };
        let key = format!("{prefix}:{}", fnv1a_hash("tok_user_1"));
        assert!(key.starts_with("rl:user:write:"));
    }

    #[test]
    fn read_key_uses_read_prefix() {
        let prefix = if false { "rl:user:write" } else { "rl:ip:read" };
        let key = format!("{prefix}:{}", fnv1a_hash("192.168.1.1"));
        assert!(key.starts_with("rl:ip:read:"));
    }

    // ── Redis integration tests ────────────────────────────────────────────

    struct TestHarness {
        _scope: crate::test_containers::TestScope,
        cache: CacheManager,
    }

    async fn make_cache() -> TestHarness {
        let scope = crate::test_containers::isolated_scope().await;
        let mut config = crate::config::Config::new_for_test();
        config.redis_url = scope.redis_url.clone();
        let pool = crate::cache::create_pool(&config).await.unwrap();
        let cache = CacheManager::new(pool, &config);
        TestHarness {
            _scope: scope,
            cache,
        }
    }

    #[tokio::test]
    async fn allows_request_under_write_limit() {
        let h = make_cache().await;
        let result = check_grpc_rate_limit(&h.cache, "grpc_user_ok", true, 10, 100).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn blocks_request_over_write_limit_with_resource_exhausted() {
        let h = make_cache().await;
        // exhaust a limit of 2
        for _ in 0..2 {
            let _ = check_grpc_rate_limit(&h.cache, "grpc_spammer", true, 2, 100).await;
        }
        let result = check_grpc_rate_limit(&h.cache, "grpc_spammer", true, 2, 100).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn allows_request_under_read_limit() {
        let h = make_cache().await;
        let result = check_grpc_rate_limit(&h.cache, "192.168.99.1", false, 10, 1000).await;
        assert!(result.is_ok());
    }
}
