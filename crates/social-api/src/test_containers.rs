//! Shared testcontainers infrastructure for unit tests.
//!
//! Provides a single Postgres and a single Redis container that are reused
//! across **all** `#[cfg(test)]` modules in the crate.  Each test still gets
//! isolation via random UUIDs — no data collisions.

use testcontainers::runners::AsyncRunner;
use tokio::sync::OnceCell;

// ── Postgres ─────────────────────────────────────────────────────────────────

pub struct SharedPg {
    _container: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    pub url: String,
}

static PG: OnceCell<SharedPg> = OnceCell::const_new();

/// Returns a long-lived Postgres container.  Migrations are run once on first
/// call; subsequent calls return the cached reference instantly.
pub async fn shared_pg() -> &'static SharedPg {
    PG.get_or_init(|| async {
        let pg = testcontainers_modules::postgres::Postgres::default()
            .start()
            .await
            .expect("postgres container");
        let port = pg.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

        // Run migrations once
        let pool = sqlx::PgPool::connect(&url).await.expect("pg connect");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool.close().await;

        SharedPg {
            _container: pg,
            url,
        }
    })
    .await
}

// ── Redis ────────────────────────────────────────────────────────────────────

pub struct SharedRedis {
    _container: testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    pub url: String,
}

static REDIS: OnceCell<SharedRedis> = OnceCell::const_new();

/// Returns a long-lived Redis container.  First call starts it; subsequent
/// calls return the cached reference instantly.
pub async fn shared_redis() -> &'static SharedRedis {
    REDIS
        .get_or_init(|| async {
            let redis = testcontainers_modules::redis::Redis::default()
                .start()
                .await
                .expect("redis container");
            let port = redis.get_host_port_ipv4(6379).await.unwrap();
            let url = format!("redis://127.0.0.1:{port}");

            SharedRedis {
                _container: redis,
                url,
            }
        })
        .await
}
